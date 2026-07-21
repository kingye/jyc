use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::Path;

use jyc_types::{load_config_layered, validation};
use jyc_utils::constants::DEFAULT_CONFIG_FILENAME;

use super::resolve::resolve_config;

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Create a default config.toml (in the platform config dir, or in
    /// --workdir when given)
    Init,
    /// Validate the current configuration
    Validate {
        /// Config file path
        #[arg(short, long)]
        config: Option<String>,
    },
}

pub async fn run(action: &ConfigAction, workdir: &Path, workdir_explicit: bool) -> Result<()> {
    match action {
        ConfigAction::Init => run_init(workdir, workdir_explicit).await,
        ConfigAction::Validate { config } => {
            run_validate(workdir, config.as_deref(), workdir_explicit).await
        }
    }
}

async fn run_init(workdir: &Path, workdir_explicit: bool) -> Result<()> {
    let config_path = if workdir_explicit {
        workdir.join(DEFAULT_CONFIG_FILENAME)
    } else {
        jyc_utils::paths::default_config_path().ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine platform config directory; pass --workdir explicitly"
            )
        })?
    };

    if config_path.exists() {
        anyhow::bail!("Config file already exists: {}", config_path.display());
    }

    if let Some(parent) = config_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    // Create skills/ and templates/ skeletons next to the config.
    if let Some(config_home) = config_path.parent() {
        for sub in ["skills", "templates"] {
            let dir = config_home.join(sub);
            tokio::fs::create_dir_all(&dir)
                .await
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
    }

    let template = include_str!("../../config.example.toml");
    tokio::fs::write(&config_path, template)
        .await
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    println!("Created {}", config_path.display());
    println!("Edit the file to configure your channels, then run: jyc config validate");
    Ok(())
}

async fn run_validate(
    workdir: &Path,
    config_file: Option<&str>,
    workdir_explicit: bool,
) -> Result<()> {
    let resolution = resolve_config(workdir, config_file, workdir_explicit)?;
    let config_path = &resolution.config_path;
    println!("Validating {}...", config_path.display());
    if let Some(global) = resolution
        .global_config_path
        .as_ref()
        .filter(|g| g.exists())
    {
        println!("(layered on global config: {})", global.display());
    }

    let config = load_config_layered(resolution.global_config_path.as_deref(), config_path)?;
    let errors = validation::validate_config(&config);

    if errors.is_empty() {
        println!("Configuration is valid.");
        println!(
            "  Channels: {}",
            config
                .channels
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("  Agent mode: {}", config.agent.mode);
        if let Some(ref inspect) = config.inspect {
            println!(
                "  Inspect: {} (bind: {})",
                if inspect.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                inspect.bind
            );
        }
        Ok(())
    } else {
        println!("Found {} validation error(s):", errors.len());
        for error in &errors {
            println!("{error}");
        }
        anyhow::bail!("configuration validation failed")
    }
}
