//! First-run config provisioning.
//!
//! Config file location resolution itself lives in
//! [`jyc_utils::config_resolve`] (shared with the `jyc-pipe` binary);
//! this module only creates the default config skeleton on first run.

use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use jyc_types::{DEFAULT_INSPECT_BIND, load_config_layered};

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

    let config_dir = res
        .config_path
        .parent()
        .context("config path has no parent directory")?;
    tokio::fs::create_dir_all(config_dir)
        .await
        .with_context(|| format!("failed to create {}", config_dir.display()))?;
    for sub in ["skills", "templates"] {
        let dir = config_dir.join(sub);
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

/// Resolve the effective `[inspect].bind` exactly as `jyc serve` does.
///
/// The config lookup is layered (`<config dir>/config.toml` plus the
/// `--data-dir` overlay when present). A missing config is not an error here:
/// dashboard clients must preserve the first-run flow, where the default bind
/// lets `ensure_serve_running` start `jyc serve`, which then provisions the
/// config and stops for editing.
pub fn resolve_inspect_bind(data_dir: &Path, data_dir_explicit: bool) -> Result<String> {
    // `resolve_config` fails only when the platform config dir is
    // undeterminable (and no data-dir-local config exists), and its advice —
    // "pass --config explicitly" — is a dead end here: dashboard clients have
    // no --config flag. The default bind is the only sensible answer.
    let resolution = match resolve_config(data_dir, None, data_dir_explicit) {
        Ok(resolution) => resolution,
        Err(err) => {
            tracing::debug!("config resolution failed, using default inspect bind: {err:#}");
            return Ok(DEFAULT_INSPECT_BIND.to_string());
        }
    };
    inspect_bind_from_resolution(&resolution)
}

/// Resolve the address a local inspect client should connect to.
///
/// An explicit `--addr` wins and is returned unchanged. Otherwise the
/// configured inspect bind is used, with a wildcard host replaced by loopback:
/// `0.0.0.0` and `[::]` are listen-any addresses, not connectable destinations.
pub fn resolve_inspect_client_addr(
    explicit_addr: Option<&str>,
    data_dir: &Path,
    data_dir_explicit: bool,
) -> Result<String> {
    let Some(addr) = explicit_addr else {
        return resolve_inspect_bind(data_dir, data_dir_explicit)
            .map(|bind| normalize_inspect_client_addr(&bind));
    };
    Ok(addr.to_string())
}

/// Replace a wildcard bind host with loopback while preserving the port.
pub fn normalize_inspect_client_addr(addr: &str) -> String {
    let Ok(socket) = addr.parse::<SocketAddr>() else {
        return addr.to_string();
    };
    if !socket.ip().is_unspecified() {
        return addr.to_string();
    }
    let ip = if socket.ip().is_ipv6() {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };
    SocketAddr::new(ip, socket.port()).to_string()
}

fn inspect_bind_from_resolution(resolution: &ConfigResolution) -> Result<String> {
    let config = if resolution.config_path.exists() {
        load_config_layered(
            resolution.global_config_path.as_deref(),
            &resolution.config_path,
        )
        .with_context(|| {
            format!(
                "failed to load inspect bind from {}",
                resolution.config_path.display()
            )
        })?
    } else if let Some(global) = resolution
        .global_config_path
        .as_ref()
        .filter(|path| path.exists())
    {
        load_config_layered(None, global)
            .with_context(|| format!("failed to load inspect bind from {}", global.display()))?
    } else {
        return Ok(DEFAULT_INSPECT_BIND.to_string());
    };

    Ok(config
        .inspect
        .map(|inspect| inspect.bind)
        .unwrap_or_else(|| DEFAULT_INSPECT_BIND.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const MINIMAL_CONFIG: &str = r#"
[agent]
enabled = true
mode = "agent"
"#;

    fn write_config(path: &std::path::Path, inspect_bind: Option<&str>) {
        let mut text = MINIMAL_CONFIG.to_string();
        if let Some(bind) = inspect_bind {
            text.push_str(&format!("\n[inspect]\nenabled = true\nbind = \"{bind}\"\n"));
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn explicit_addr_wins_without_reading_config() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_inspect_client_addr(Some("127.0.0.1:9816"), tmp.path(), true).unwrap(),
            "127.0.0.1:9816"
        );
    }

    #[test]
    fn explicit_addr_is_returned_unchanged() {
        assert_eq!(
            normalize_inspect_client_addr("127.0.0.1:9816"),
            "127.0.0.1:9816"
        );
        assert_eq!(
            normalize_inspect_client_addr("localhost:9816"),
            "localhost:9816"
        );
    }

    #[test]
    fn wildcard_binds_become_loopback_clients() {
        assert_eq!(
            normalize_inspect_client_addr("0.0.0.0:9816"),
            "127.0.0.1:9816"
        );
        assert_eq!(normalize_inspect_client_addr("[::]:9816"), "[::1]:9816");
    }

    #[test]
    fn missing_config_uses_the_default_bind() {
        let resolution = ConfigResolution {
            config_path: PathBuf::from("/nonexistent/config.toml"),
            global_config_path: None,
            is_default: false,
        };
        assert_eq!(
            inspect_bind_from_resolution(&resolution).unwrap(),
            DEFAULT_INSPECT_BIND
        );
    }

    #[test]
    fn client_addr_resolves_through_the_real_config_path() {
        // Goes through `resolve_config` + `inspect_bind_from_resolution`
        // against a config on disk, so a swapped `data_dir_explicit` plumbing
        // in main.rs cannot pass unnoticed.
        let tmp = tempfile::tempdir().unwrap();
        write_config(&tmp.path().join("config.toml"), Some("0.0.0.0:9816"));
        assert_eq!(
            resolve_inspect_client_addr(None, tmp.path(), true).unwrap(),
            "127.0.0.1:9816"
        );
    }

    #[test]
    fn config_without_an_inspect_section_uses_the_default_bind() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(&tmp.path().join("config.toml"), None);
        assert_eq!(
            resolve_inspect_client_addr(None, tmp.path(), true).unwrap(),
            DEFAULT_INSPECT_BIND
        );
    }

    #[test]
    fn data_dir_config_supplies_the_inspect_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        write_config(&config_path, Some("0.0.0.0:9816"));

        let bind = inspect_bind_from_resolution(&ConfigResolution {
            config_path,
            global_config_path: None,
            is_default: false,
        })
        .unwrap();
        assert_eq!(bind, "0.0.0.0:9816");
    }

    #[test]
    fn global_config_is_the_base_layer_and_data_dir_overrides_it() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global.toml");
        let local = tmp.path().join("config.toml");
        write_config(&global, Some("127.0.0.1:1111"));
        write_config(&local, Some("127.0.0.1:2222"));

        let resolution = ConfigResolution {
            config_path: local.clone(),
            global_config_path: Some(global),
            is_default: false,
        };
        assert_eq!(
            inspect_bind_from_resolution(&resolution).unwrap(),
            "127.0.0.1:2222"
        );

        // Without a data-dir-local [inspect], the global bind remains effective.
        write_config(&local, None);
        assert_eq!(
            inspect_bind_from_resolution(&resolution).unwrap(),
            "127.0.0.1:1111"
        );
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
