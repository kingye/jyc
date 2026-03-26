use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::Path;

use crate::config::{load_config, validation};
use crate::utils::constants::DEFAULT_CONFIG_FILENAME;

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Create a default config.toml in the working directory
    Init,
    /// Validate the current configuration
    Validate {
        /// Config file path
        #[arg(short, long, default_value = DEFAULT_CONFIG_FILENAME)]
        config: String,
    },
}

pub async fn run(action: &ConfigAction, workdir: &Path) -> Result<()> {
    match action {
        ConfigAction::Init => run_init(workdir).await,
        ConfigAction::Validate { config } => run_validate(workdir, config).await,
    }
}

async fn run_init(workdir: &Path) -> Result<()> {
    let config_path = workdir.join(DEFAULT_CONFIG_FILENAME);

    if config_path.exists() {
        anyhow::bail!(
            "Config file already exists: {}",
            config_path.display()
        );
    }

    let template = include_str!("../../config_template.toml");
    tokio::fs::write(&config_path, template)
        .await
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    println!("Created {}", config_path.display());
    println!("Edit the file to configure your channels, then run: jyc config validate");
    Ok(())
}

async fn run_validate(workdir: &Path, config_file: &str) -> Result<()> {
    let config_path = workdir.join(config_file);
    println!("Validating {}...", config_path.display());

    let config = load_config(&config_path)?;
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
        if let Some(ref alerting) = config.alerting {
            println!(
                "  Alerting: {} (→ {})",
                if alerting.enabled { "enabled" } else { "disabled" },
                alerting.recipient
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
