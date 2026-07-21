//! Config file location resolution and first-run provisioning.
//!
//! Layering model (see issue #393):
//! - **L1 (global)**: `<config_home>/config.toml` (e.g. `~/.config/jyc/config.toml`)
//! - **L2 (workdir/data root)**: `--config`/`--workdir` config, merged on top of L1

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jyc_utils::constants::DEFAULT_CONFIG_FILENAME;
use jyc_utils::paths;

/// Result of resolving config file locations for serve/config commands.
#[derive(Debug, Clone)]
pub struct ConfigResolution {
    /// Effective config file (L2 overlay, or the L1 file itself on default invocation).
    pub config_path: PathBuf,
    /// Global (L1) config used as base layer, when it differs from `config_path`.
    pub global_config_path: Option<PathBuf>,
    /// True when neither `--workdir` nor `--config` was given (default invocation).
    pub is_default: bool,
}

/// Resolve the effective config path and the optional global (L1) layer.
///
/// Rules:
/// - `--config <path>`: absolute paths used as-is; `~` is expanded; **relative
///   paths are resolved against the current directory** (the user's shell cwd).
///   L1 still applies as base layer when different.
/// - No `--config`, explicit `--workdir`: `<workdir>/config.toml`, L1 as base.
/// - No `--config`, no `--workdir`: `<config_home>/config.toml` (is_default).
pub fn resolve_config(
    workdir: &Path,
    config_arg: Option<&str>,
    workdir_explicit: bool,
) -> Result<ConfigResolution> {
    let global = paths::default_config_path();

    let config_path = match config_arg {
        Some(c) => {
            let expanded = paths::expand_tilde(c);
            if expanded.is_absolute() {
                expanded
            } else {
                // Resolve relative --config against the shell's cwd, not the
                // workdir (a flag typed in the terminal is a cwd-relative path).
                std::env::current_dir()
                    .unwrap_or_else(|_| workdir.to_path_buf())
                    .join(expanded)
            }
        }
        None if workdir_explicit => workdir.join(DEFAULT_CONFIG_FILENAME),
        None => global.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine platform config directory; pass --config explicitly"
            )
        })?,
    };

    Ok(ConfigResolution {
        global_config_path: global.filter(|g| *g != config_path),
        config_path,
        is_default: config_arg.is_none() && !workdir_explicit,
    })
}

/// Create the default config (plus `skills/` and `templates/` skeletons) on
/// first run of a default invocation.
///
/// Returns `Ok(true)` when the config was created — the caller should stop
/// and let the user edit the file before starting again.
pub async fn provision_default_config(res: &ConfigResolution) -> Result<bool> {
    if !res.is_default || res.config_path.exists() {
        return Ok(false);
    }

    let config_home = res
        .config_path
        .parent()
        .context("config path has no parent directory")?;
    tokio::fs::create_dir_all(config_home)
        .await
        .with_context(|| format!("failed to create {}", config_home.display()))?;
    for sub in ["skills", "templates"] {
        let dir = config_home.join(sub);
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }

    let template = include_str!("../../config.example.toml");
    tokio::fs::write(&res.config_path, template)
        .await
        .with_context(|| format!("failed to write {}", res.config_path.display()))?;

    println!(
        "Created default configuration: {}",
        res.config_path.display()
    );
    println!("Edit the file to configure your channels, then run `jyc serve` again.");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_config_explicit_relative_against_workdir() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let res = resolve_config(Path::new("/data"), Some("custom.toml"), true).unwrap();
        assert_eq!(res.config_path, cwd.join("custom.toml"));
        assert!(!res.is_default);
    }

    #[test]
    fn test_resolve_config_explicit_absolute() {
        let res = resolve_config(Path::new("/data"), Some("/etc/jyc.toml"), true).unwrap();
        assert_eq!(res.config_path, PathBuf::from("/etc/jyc.toml"));
        assert!(!res.is_default);
    }

    #[test]
    fn test_resolve_config_workdir_only() {
        let res = resolve_config(Path::new("/data"), None, true).unwrap();
        assert_eq!(res.config_path, PathBuf::from("/data/config.toml"));
        assert!(!res.is_default);
    }

    #[test]
    fn test_resolve_config_default_invocation() {
        let res = resolve_config(Path::new("/data"), None, false).unwrap();
        assert!(res.is_default);
        assert!(res.global_config_path.is_none());
        if let Some(expected) = paths::default_config_path() {
            assert_eq!(res.config_path, expected);
        }
    }

    #[tokio::test]
    async fn test_provision_skips_non_default_invocation() {
        let res = ConfigResolution {
            config_path: PathBuf::from("/nonexistent/config.toml"),
            global_config_path: None,
            is_default: false,
        };
        assert!(!provision_default_config(&res).await.unwrap());
    }

    #[tokio::test]
    async fn test_provision_creates_config_and_skeletons() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let res = ConfigResolution {
            config_path: config_path.clone(),
            global_config_path: None,
            is_default: true,
        };
        assert!(provision_default_config(&res).await.unwrap());
        assert!(config_path.exists(), "config.toml should be created");
        assert!(tmp.path().join("skills").is_dir(), "skills/ should exist");
        assert!(
            tmp.path().join("templates").is_dir(),
            "templates/ should exist"
        );
        // Second call is a no-op
        assert!(!provision_default_config(&res).await.unwrap());
    }
}
