//! Config file location resolution (shared by `jyc` and `jyc-pipe`).
//!
//! Layering model (see issue #393):
//! - **L1 (global)**: `<config_home>/config.toml` (e.g. `~/.config/jyc/config.toml`)
//! - **L2 (workdir/data root)**: `--config`/`--workdir` config, merged on top of L1

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::constants::DEFAULT_CONFIG_FILENAME;
use crate::paths;

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

/// Resolve the working directory / data root: the explicit `--workdir`
/// (tilde-expanded, canonicalized when possible) or the platform data dir
/// (e.g. `~/.local/share/jyc` on Linux).
pub fn resolve_workdir(workdir: Option<&PathBuf>) -> Result<PathBuf> {
    match workdir {
        Some(w) => {
            let expanded = paths::expand_tilde(&w.to_string_lossy());
            let abs = std::fs::canonicalize(&expanded).unwrap_or(expanded);
            Ok(abs)
        }
        None => paths::data_home().ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine platform data directory; pass --workdir explicitly"
            )
        }),
    }
}

/// Resolve the effective config path and the optional global (L1) layer.
///
/// Rules:
/// - `--config <path>`: absolute paths used as-is; `~` is expanded; **relative
///   paths are resolved against the current directory** (the user's shell cwd).
///   L1 still applies as base layer when different.
/// - No `--config`, explicit `--workdir`: `<workdir>/config.toml` if it exists,
///   otherwise fall back to `<config_home>/config.toml` (L1 global). L1 as base.
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
        None if workdir_explicit => {
            let candidate = workdir.join(DEFAULT_CONFIG_FILENAME);
            if candidate.exists() {
                candidate
            } else {
                // No workdir-local config - fall back to the platform
                // global (L1). L2 is an optional overlay, not required.
                global.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "no config found at {} or in the platform config directory; \
                         pass --config explicitly",
                        candidate.display()
                    )
                })?
            }
        }
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
    fn test_resolve_config_workdir_only_with_local_config() {
        // When `<workdir>/config.toml` exists, it is used (L2 overlay
        // over L1 global).
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path();
        std::fs::write(workdir.join("config.toml"), "[general]\n").unwrap();

        let res = resolve_config(workdir, None, true).unwrap();
        assert_eq!(res.config_path, workdir.join("config.toml"));
        assert!(!res.is_default);
    }

    #[test]
    fn test_resolve_config_workdir_only_falls_back_to_global() {
        // When `<workdir>/config.toml` does NOT exist, fall back to the
        // platform global (L1) so a workdir-only invocation still works
        // for users who only configured `~/.config/jyc/config.toml`.
        let tmp = tempfile::tempdir().unwrap();
        let res = resolve_config(tmp.path(), None, true).unwrap();
        // config_path is either global (most cases) or tmp/config.toml if
        // somehow the global resolves there. Either way: not the workdir
        // path because the file doesn't exist.
        assert_ne!(res.config_path, tmp.path().join("config.toml"));
        // Global should be set as base layer when not the same file.
        if let Some(expected) = paths::default_config_path() {
            assert_eq!(res.config_path, expected);
        }
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
}
