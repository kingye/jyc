//! First-run config provisioning.
//!
//! Config file location resolution itself lives in
//! [`jyc_utils::config_resolve`] (shared with the `jyc-pipe` binary);
//! this module only creates the default config skeleton on first run.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub use jyc_utils::config_resolve::{ConfigResolution, resolve_config};

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
