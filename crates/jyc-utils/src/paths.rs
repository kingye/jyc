//! Centralized platform path resolution.
//!
//! JYC follows platform conventions for user-edited configuration and
//! generated data:
//!
//! | Platform | Config dir                    | Data dir                        |
//! |----------|-------------------------------|---------------------------------|
//! | Linux    | `$XDG_CONFIG_HOME/jyc`   | `$XDG_DATA_HOME/jyc`        |
//! |          | (`~/.config/jyc`)        | (`~/.local/share/jyc`)      |
//! | macOS    | `$XDG_CONFIG_HOME/jyc`   | `$XDG_DATA_HOME/jyc`        |
//! |          | (`~/.config/jyc`)        | (`~/.local/share/jyc`)      |
//! | Windows  | `%APPDATA%\jyc`          | `%LOCALAPPDATA%\jyc`        |
//!
//! On Unix (Linux / macOS) the XDG base directory convention is used
//! (`~/.config/jyc` and `~/.local/share/jyc`). On Windows the native
//! `dirs` crate paths are used.
//!
//! The **config dir** (L1) holds user-edited files: `config.toml`,
//! `skills/`, `templates/`. The **data dir** (default workdir, L2) holds
//! generated state: channel state, workspaces, threads.

use std::path::PathBuf;

use crate::constants::DEFAULT_CONFIG_FILENAME;

/// Directory name appended to platform config/data dirs.
pub const APP_DIR_NAME: &str = "jyc";

/// User-edited configuration directory (L1).
///
/// Linux/macOS: `$XDG_CONFIG_HOME/jyc` or `~/.config/jyc`.
/// Windows: `%APPDATA%\jyc`.
/// Returns `None` when no home/config directory can be determined.
pub fn config_home() -> Option<PathBuf> {
    #[cfg(not(windows))]
    {
        // XDG convention on all Unix (Linux + macOS)
        Some(
            std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .ok()
                .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?
                .join(APP_DIR_NAME),
        )
    }
    #[cfg(windows)]
    {
        dirs::config_dir().map(|p| p.join(APP_DIR_NAME))
    }
}

/// Generated-data directory (default workdir / data root, L2).
///
/// Linux/macOS: `$XDG_DATA_HOME/jyc` or `~/.local/share/jyc`.
/// Windows: `%LOCALAPPDATA%\jyc`.
/// Returns `None` when no home/data directory can be determined.
pub fn data_home() -> Option<PathBuf> {
    #[cfg(not(windows))]
    {
        Some(
            std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .ok()
                .or_else(|| {
                    let home = dirs::home_dir()?;
                    Some(home.join(".local").join("share"))
                })?
                .join(APP_DIR_NAME),
        )
    }
    #[cfg(windows)]
    {
        dirs::data_local_dir().map(|p| p.join(APP_DIR_NAME))
    }
}

/// Default config file path: `<config_home>/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    config_home().map(|p| p.join(DEFAULT_CONFIG_FILENAME))
}

/// Default skills directory: `<config_home>/skills`.
pub fn global_skills_dir() -> Option<PathBuf> {
    config_home().map(|p| p.join("skills"))
}

/// Default templates directory: `<config_home>/templates`.
pub fn global_templates_dir() -> Option<PathBuf> {
    config_home().map(|p| p.join("templates"))
}

/// Expand a leading `~` to the user's home directory.
///
/// Absolute paths and paths without a `~` prefix are returned unchanged.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_home_ends_with_jyc() {
        if let Some(p) = config_home() {
            assert_eq!(p.file_name().unwrap(), APP_DIR_NAME);
        }
    }

    #[test]
    fn test_data_home_ends_with_jyc() {
        if let Some(p) = data_home() {
            assert_eq!(p.file_name().unwrap(), APP_DIR_NAME);
        }
    }

    #[test]
    fn test_default_config_path() {
        if let Some(p) = default_config_path() {
            assert!(p.ends_with(format!("jyc/{DEFAULT_CONFIG_FILENAME}")));
        }
    }

    #[test]
    fn test_expand_tilde_plain() {
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
    }

    #[test]
    fn test_expand_tilde_home() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde("~"), home);
            assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
        }
    }
}
