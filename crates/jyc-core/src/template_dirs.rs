//! Layered template directory resolution.
//!
//! Templates are looked up across three levels (see issue #393):
//! - **L1 (global)**: `<config_home>/templates/` (e.g. `~/.config/jyc/templates`)
//! - **L2 (workdir/data root)**: `<workdir>/templates/`
//! - **L3 (thread)**: `<thread_path>/.jyc/templates/`
//!
//! Higher levels win when a template with the same name exists in multiple
//! levels.

use std::path::{Path, PathBuf};

/// Ordered template directories, low → high priority.
#[derive(Debug, Clone, Default)]
pub struct TemplateDirs(Vec<PathBuf>);

impl TemplateDirs {
    /// Create from a list of directories ordered low → high priority.
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self(dirs)
    }

    /// Create with a single directory (e.g. tests, legacy callers).
    pub fn single(dir: PathBuf) -> Self {
        Self(vec![dir])
    }

    /// Resolve a template by name, searching high → low priority.
    ///
    /// Returns the directory containing the template (i.e.
    /// `<dir>/<name>`), or `None` when the template is not found.
    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        for dir in self.0.iter().rev() {
            let candidate = dir.join(name);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
        None
    }

    /// Resolve a template by name, checking the thread-level (L3)
    /// `.jyc/templates/` directory first, then the configured layers.
    pub fn resolve_with_thread(&self, thread_path: &Path, name: &str) -> Option<PathBuf> {
        let thread_level = thread_path.join(".jyc").join("templates").join(name);
        if thread_level.is_dir() {
            return Some(thread_level);
        }
        self.resolve(name)
    }
}

impl From<PathBuf> for TemplateDirs {
    fn from(dir: PathBuf) -> Self {
        Self::single(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_template(base: &Path, name: &str) {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "body").unwrap();
    }

    #[test]
    fn test_resolve_single_dir() {
        let tmp = tempfile::tempdir().unwrap();
        make_template(tmp.path(), "alpha");
        let dirs = TemplateDirs::single(tmp.path().to_path_buf());
        assert_eq!(dirs.resolve("alpha"), Some(tmp.path().join("alpha")));
        assert_eq!(dirs.resolve("missing"), None);
    }

    #[test]
    fn test_resolve_higher_priority_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let workdir = tmp.path().join("workdir");
        make_template(&global, "alpha");
        make_template(&workdir, "alpha");
        make_template(&global, "beta");

        let dirs = TemplateDirs::new(vec![global.clone(), workdir.clone()]);
        // Present in both → workdir (higher priority) wins
        assert_eq!(dirs.resolve("alpha"), Some(workdir.join("alpha")));
        // Present only in global → found there
        assert_eq!(dirs.resolve("beta"), Some(global.join("beta")));
    }

    #[test]
    fn test_resolve_with_thread_level_first() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path().join("workdir");
        let thread = tmp.path().join("thread");
        make_template(&workdir, "alpha");
        make_template(&thread.join(".jyc").join("templates"), "alpha");
        make_template(&thread.join(".jyc").join("templates"), "gamma");

        let dirs = TemplateDirs::single(workdir.clone());
        // Thread level wins over workdir
        assert_eq!(
            dirs.resolve_with_thread(&thread, "alpha"),
            Some(thread.join(".jyc/templates/alpha"))
        );
        // Only at thread level
        assert_eq!(
            dirs.resolve_with_thread(&thread, "gamma"),
            Some(thread.join(".jyc/templates/gamma"))
        );
        // Only at workdir level
        make_template(&workdir, "beta");
        assert_eq!(
            dirs.resolve_with_thread(&thread, "beta"),
            Some(workdir.join("beta"))
        );
    }
}
