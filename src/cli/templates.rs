use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::core::template_utils::overwrite_template_files;

/// Actions for the `templates` subcommand.
#[derive(Debug, Subcommand)]
pub enum TemplatesAction {
    /// List available templates and their skills
    List {
        /// Path to the source directory containing templates/ and .opencode/skills/
        #[arg(long)]
        source_dir: Option<PathBuf>,
    },
    /// Deploy one or all templates to a target directory
    Deploy {
        /// Target directory to deploy templates into
        target_dir: PathBuf,

        /// Deploy only this template (omit to deploy all)
        template_name: Option<String>,

        /// Rename the deployed template directory
        #[arg(long = "as")]
        as_name: Option<String>,

        /// Write a model override file into the deployed template
        #[arg(long)]
        model: Option<String>,

        /// Override MCPs for the deployed template (comma-separated)
        #[arg(long, value_delimiter = ',')]
        mcps: Option<Vec<String>>,

        /// Path to the source directory containing templates/ and .opencode/skills/
        #[arg(long)]
        source_dir: Option<PathBuf>,
    },
}

/// A single template entry in `templates.toml`.
#[derive(Debug, Deserialize)]
struct TemplateEntry {
    skills: Vec<String>,
    #[serde(default)]
    mcps: Vec<String>,
}

/// Top-level structure of `templates/templates.toml`.
#[derive(Debug, Deserialize)]
struct TemplatesConfig {
    templates: HashMap<String, TemplateEntry>,
}

/// Dispatch to the appropriate subcommand handler.
pub async fn run(action: &TemplatesAction, _workdir: &Path) -> Result<()> {
    match action {
        TemplatesAction::List { source_dir } => run_list(source_dir.as_deref()).await,
        TemplatesAction::Deploy {
            target_dir,
            template_name,
            as_name,
            model,
            mcps,
            source_dir,
        } => {
            run_deploy(
                target_dir,
                template_name.as_deref(),
                as_name.as_deref(),
                model.as_deref(),
                mcps.as_deref(),
                source_dir.as_deref(),
            )
            .await
        }
    }
}

/// Resolve the source directory that contains `templates/` and `.opencode/skills/`.
///
/// Priority:
/// 1. Explicit `--source-dir` argument
/// 2. Walk up from the executable location looking for `Cargo.toml` + `templates/`
/// 3. Fall back to the current working directory
fn resolve_source_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        debug!("using explicit source dir: {}", dir.display());
        return Ok(dir.to_path_buf());
    }

    // Walk up from the executable location
    if let Ok(exe) = std::env::current_exe() {
        let mut candidate = exe.parent().map(Path::to_path_buf);
        while let Some(dir) = candidate {
            if dir.join("Cargo.toml").exists() && dir.join("templates").is_dir() {
                debug!("auto-detected source dir: {}", dir.display());
                return Ok(dir);
            }
            candidate = dir.parent().map(Path::to_path_buf);
        }
    }

    // Fall back to current working directory
    let cwd = std::env::current_dir().context("failed to get current working directory")?;
    debug!("falling back to cwd as source dir: {}", cwd.display());
    Ok(cwd)
}

/// Load and parse `templates/templates.toml` from the source directory.
async fn load_templates_config(source_dir: &Path) -> Result<TemplatesConfig> {
    let config_path = source_dir.join("templates").join("templates.toml");
    let content = tokio::fs::read_to_string(&config_path)
        .await
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config: TemplatesConfig =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", config_path.display()))?;
    Ok(config)
}

/// List all available templates and their configured skills.
async fn run_list(source_dir_arg: Option<&Path>) -> Result<()> {
    let source_dir = resolve_source_dir(source_dir_arg)?;
    let config = load_templates_config(&source_dir).await?;
    let templates_dir = source_dir.join("templates");

    let mut entries = tokio::fs::read_dir(&templates_dir)
        .await
        .with_context(|| format!("failed to read {}", templates_dir.display()))?;

    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();

    for name in &names {
        let skills = config
            .templates
            .get(name.as_str())
            .map(|e| e.skills.join(", "))
            .unwrap_or_else(|| "(no skills)".to_string());
        let mcps = config
            .templates
            .get(name.as_str())
            .map(|e| e.mcps.join(", "))
            .unwrap_or_default();
        println!("{name}: {skills}");
        if !mcps.is_empty() {
            println!("  mcps: {mcps}");
        }
    }

    println!("\n{} template(s) total.", names.len());
    Ok(())
}

/// Deploy one or all templates to the target directory.
async fn run_deploy(
    target_dir: &Path,
    template_name: Option<&str>,
    as_name: Option<&str>,
    model: Option<&str>,
    mcps_override: Option<&[String]>,
    source_dir_arg: Option<&Path>,
) -> Result<()> {
    let source_dir = resolve_source_dir(source_dir_arg)?;
    let config = load_templates_config(&source_dir).await?;
    let templates_dir = source_dir.join("templates");
    let skills_dir = source_dir.join(".opencode").join("skills");

    // Validate: --as requires a template name
    if as_name.is_some() && template_name.is_none() {
        anyhow::bail!("--as requires a template name to be specified");
    }

    // Validate: template exists if specified
    if let Some(name) = template_name {
        let template_path = templates_dir.join(name);
        if !template_path.is_dir() {
            anyhow::bail!(
                "template '{}' not found at {}",
                name,
                template_path.display()
            );
        }
    }

    println!("=== Template Deployment ===");
    println!("Source templates: {}", templates_dir.display());
    println!("Source skills:    {}", skills_dir.display());
    println!("Target:           {}", target_dir.display());
    if let Some(name) = template_name {
        println!("Template filter: {name}");
    }
    if let Some(name) = as_name {
        println!("Deploying as:    {name}");
    }
    if let Some(m) = model {
        println!("Model override:  {m}");
    }
    if let Some(m) = mcps_override {
        println!("MCPs override:   {}", m.join(", "));
    }
    println!();

    // Collect and sort template directories
    let mut entries = tokio::fs::read_dir(&templates_dir)
        .await
        .with_context(|| format!("failed to read {}", templates_dir.display()))?;

    let mut template_dirs = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                template_dirs.push(name.to_string());
            }
        }
    }
    template_dirs.sort();

    for tpl_name in &template_dirs {
        // Filter by template name if specified
        if let Some(filter) = template_name {
            if tpl_name != filter {
                continue;
            }
        }

        let deploy_name = as_name.unwrap_or(tpl_name.as_str());
        let target = target_dir.join(deploy_name);
        let tpl_src = templates_dir.join(tpl_name);

        println!("--- {tpl_name} ---");
        if as_name.is_some() {
            println!("  deploying as: {deploy_name}");
        }

        tokio::fs::create_dir_all(&target)
            .await
            .with_context(|| format!("failed to create {}", target.display()))?;

        // Copy AGENTS.md
        let agents_src = tpl_src.join("AGENTS.md");
        if agents_src.exists() {
            tokio::fs::copy(&agents_src, target.join("AGENTS.md"))
                .await
                .with_context(|| format!("failed to copy AGENTS.md for {tpl_name}"))?;
            println!("  AGENTS.md copied");
        }

        // Copy .jyc/ directory if it exists
        let jyc_src = tpl_src.join(".jyc");
        if jyc_src.is_dir() {
            let jyc_dst = target.join(".jyc");
            // Remove existing .jyc/ to ensure clean copy
            if jyc_dst.exists() {
                tokio::fs::remove_dir_all(&jyc_dst).await.ok();
            }
            overwrite_template_files(&jyc_src, &jyc_dst).await?;
            println!("  .jyc copied");
        }

        // Copy skills
        let skills = config
            .templates
            .get(tpl_name.as_str())
            .map(|e| e.skills.as_slice())
            .unwrap_or(&[]);

        if skills.is_empty() {
            println!("  (no skills)");
        } else {
            let skills_target = target.join(".opencode").join("skills");
            tokio::fs::create_dir_all(&skills_target)
                .await
                .with_context(|| format!("failed to create skills dir for {tpl_name}"))?;

            for skill in skills {
                let skill_src = skills_dir.join(skill);
                if skill_src.is_dir() {
                    let skill_dst = skills_target.join(skill);
                    // Remove existing skill dir for clean copy
                    if skill_dst.exists() {
                        tokio::fs::remove_dir_all(&skill_dst).await.ok();
                    }
                    overwrite_template_files(&skill_src, &skill_dst).await?;
                    println!("  skill: {skill}");
                } else {
                    println!("  WARNING: skill '{skill}' not found at {}", skill_src.display());
                }
            }
        }

        // Write model override if specified
        if let Some(m) = model {
            let jyc_dir = target.join(".jyc");
            tokio::fs::create_dir_all(&jyc_dir)
                .await
                .with_context(|| format!("failed to create .jyc dir for {tpl_name}"))?;
            tokio::fs::write(jyc_dir.join("model-override"), m)
                .await
                .with_context(|| format!("failed to write model-override for {tpl_name}"))?;
            println!("  model-override: {m}");
        }

        // Write mcps.json for template
        let mcps = mcps_override
            .map(|m| m.to_vec())
            .unwrap_or_else(|| {
                config
                    .templates
                    .get(tpl_name.as_str())
                    .map(|e| e.mcps.clone())
                    .unwrap_or_default()
            });

        if !mcps.is_empty() {
            let jyc_dir = target.join(".jyc");
            tokio::fs::create_dir_all(&jyc_dir)
                .await
                .with_context(|| format!("failed to create .jyc dir for {tpl_name}"))?;
            let mcps_json = serde_json::to_string_pretty(&mcps)?;
            tokio::fs::write(jyc_dir.join("mcps.json"), mcps_json)
                .await
                .with_context(|| format!("failed to write mcps.json for {tpl_name}"))?;
            println!("  mcps: {}", mcps.join(", "));
        }
    }

    println!();
    println!("=== Deployment complete ===");
    println!("Templates deployed to: {}", target_dir.display());
    Ok(())
}
