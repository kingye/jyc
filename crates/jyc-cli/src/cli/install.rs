//! Shared logic for the `agents` and `skills` subcommands.
//!
//! Both commands install named directories from a *source* directory
//! (e.g. the jyc repo checkout) into a *target* directory (a config or
//! workdir layer):
//!
//! - `jyc agents` operates on `<source>/templates/` → `<target>/templates/`
//! - `jyc skills` operates on `<source>/skills/` → `<target>/skills/`

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use jyc_core::template_utils::overwrite_template_files;

/// Resolve the source directory: explicit `--source`, else the current
/// working directory.
pub fn resolve_source(explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        Some(d) => Ok(jyc_utils::paths::expand_tilde(&d.to_string_lossy())),
        None => std::env::current_dir().context("failed to get current working directory"),
    }
}

/// Resolve the target directory: explicit `--target`, else the platform
/// config home (e.g. `~/.config/jyc`).
pub fn resolve_target(explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        Some(d) => Ok(jyc_utils::paths::expand_tilde(&d.to_string_lossy())),
        None => jyc_utils::paths::config_home().ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine platform config directory; pass --target explicitly"
            )
        }),
    }
}

/// List the names of subdirectories in `<source>/<collection>/`.
///
/// Returns an error when the collection directory does not exist.
pub async fn list_entries(source: &Path, collection: &str) -> Result<Vec<String>> {
    let dir = source.join(collection);
    let mut entries = tokio::fs::read_dir(&dir)
        .await
        .with_context(|| format!("failed to read {} (is --source correct?)", dir.display()))?;

    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.path().is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Install one entry (or all entries when `name` is `None`) from
/// `<source>/<collection>/` into `<target>/<collection>/`.
///
/// Existing target entries are replaced with a clean copy.
pub async fn install_entries(
    source: &Path,
    target: &Path,
    collection: &str,
    name: Option<&str>,
) -> Result<()> {
    let available = list_entries(source, collection).await?;

    let selected: Vec<&str> = match name {
        Some(n) => {
            if !available.iter().any(|a| a == n) {
                anyhow::bail!(
                    "'{n}' not found at {}/{collection}/ (available: {})",
                    source.display(),
                    available.join(", ")
                );
            }
            vec![n]
        }
        None => available.iter().map(String::as_str).collect(),
    };

    let src_dir = source.join(collection);
    let dst_dir = target.join(collection);
    println!("Source: {}", src_dir.display());
    println!("Target: {}", dst_dir.display());

    for entry_name in &selected {
        let src = src_dir.join(entry_name);
        let dst = dst_dir.join(entry_name);
        // Remove existing entry for a clean copy
        if dst.exists() {
            tokio::fs::remove_dir_all(&dst).await.ok();
        }
        overwrite_template_files(&src, &dst)
            .await
            .with_context(|| format!("failed to install '{entry_name}'"))?;
        println!("  installed: {entry_name}");
    }

    let noun = match (selected.len(), collection) {
        (1, "templates") => "template",
        (1, "skills") => "skill",
        (_, c) => c,
    };
    println!("{} {noun} installed.", selected.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake source dir with two entries in the given collection.
    async fn make_source(tmp: &Path, collection: &str, names: &[&str]) -> PathBuf {
        let source = tmp.join("source");
        for name in names {
            let dir = source.join(collection).join(name);
            tokio::fs::create_dir_all(&dir).await.unwrap();
            tokio::fs::write(dir.join("AGENTS.md"), name).await.unwrap();
        }
        source
    }

    #[tokio::test]
    async fn install_all_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), "templates", &["alpha", "beta"]).await;
        let target = tmp.path().join("target");

        install_entries(&source, &target, "templates", None)
            .await
            .unwrap();

        assert!(target.join("templates/alpha/AGENTS.md").exists());
        assert!(target.join("templates/beta/AGENTS.md").exists());
    }

    #[tokio::test]
    async fn install_single_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), "skills", &["alpha", "beta"]).await;
        let target = tmp.path().join("target");

        install_entries(&source, &target, "skills", Some("alpha"))
            .await
            .unwrap();

        assert!(target.join("skills/alpha/AGENTS.md").exists());
        assert!(!target.join("skills/beta").exists());
    }

    #[tokio::test]
    async fn install_missing_entry_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), "templates", &["alpha"]).await;
        let target = tmp.path().join("target");

        let err = install_entries(&source, &target, "templates", Some("missing"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'missing' not found"));
    }

    #[tokio::test]
    async fn list_entries_missing_collection_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = list_entries(tmp.path(), "templates").await.unwrap_err();
        assert!(err.to_string().contains("failed to read"));
    }

    #[test]
    fn resolve_source_explicit() {
        let p = resolve_source(Some(Path::new("/tmp/x"))).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn resolve_target_explicit() {
        let p = resolve_target(Some(Path::new("/tmp/y"))).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/y"));
    }
}
