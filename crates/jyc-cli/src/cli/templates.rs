use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::debug;

use jyc_core::template_utils::overwrite_template_files;
use jyc_types::McpServerConfig;

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

        /// Agent profile file (TOML) with per-template skills, MCPs, and context
        #[arg(long)]
        profile: Option<PathBuf>,

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
            profile,
            source_dir,
        } => {
            run_deploy(
                target_dir,
                template_name.as_deref(),
                as_name.as_deref(),
                model.as_deref(),
                profile.as_deref(),
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

/// Agent profile config: MCP definitions + per-template skills, MCPs, and context.
///
/// This is passed via `--profile` (or deprecated alias `--mcps`) at deploy time.
/// It allows attaching per-template capabilities to each agent template.
#[derive(Debug, Deserialize, Default)]
struct ProfileConfig {
    /// MCP server definitions to be written into deployed templates.
    #[serde(default)]
    mcps: Vec<McpServerConfig>,
    #[serde(default)]
    templates: HashMap<String, ProfileEntry>,
}

/// Per-template profile entry declaring which skills, MCPs, and context to attach.
#[derive(Debug, Deserialize, Default)]
struct ProfileEntry {
    /// Extra MCPs to enable for this template.
    #[serde(default)]
    mcps: Vec<String>,
    /// Extra skills to deploy for this template (merged with templates.toml skills).
    #[serde(default)]
    skills: Vec<String>,
    /// Context files to append to AGENTS.md (paths relative to the profile file).
    /// Each file's content is appended as a new section in the deployed AGENTS.md.
    #[serde(default)]
    context_files: Vec<String>,
}

/// Load agent profile config from a TOML file.
///
/// The file format:
/// ```toml
/// [[mcps]]
/// name = "my-mcp"
/// type = "remote"
/// url = "https://mcp.example.com/mcp"
/// enabled = true
///
/// [templates.github-planner]
/// mcps = ["my-mcp"]
/// skills = ["specific-skill"]
/// context_files = ["context/specific-context.md"]
/// ```
async fn load_profile_config(path: &Path) -> Result<ProfileConfig> {
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    let config: ProfileConfig =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(config)
}

/// Resolve a template's effective profile entry.
///
/// Returns the profile entry for the given template, or a default empty one if not found.
fn resolve_profile_entry(
    profile: &ProfileConfig,
    tpl_name: &str,
) -> ProfileEntry {
    let base = profile.templates.get(tpl_name);

    ProfileEntry {
        mcps: base.map(|e| e.mcps.clone()).unwrap_or_default(),
        skills: base.map(|e| e.skills.clone()).unwrap_or_default(),
        context_files: base.map(|e| e.context_files.clone()).unwrap_or_default(),
    }
}

/// Deploy one or all templates to the target directory.
async fn run_deploy(
    target_dir: &Path,
    template_name: Option<&str>,
    as_name: Option<&str>,
    model: Option<&str>,
    profile_file: Option<&Path>,
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
    let profile = if let Some(path) = profile_file {
        println!("Profile:         {}", path.display());
        load_profile_config(path).await?
    } else {
        ProfileConfig::default()
    };

    // Validate profile: warn about MCP names referenced in templates but not defined
    if profile_file.is_some() {
        let defined_mcp_names: Vec<&str> = profile.mcps.iter().map(|m| m.name.as_str()).collect();
        for (tpl_name, entry) in &profile.templates {
            for mcp_name in &entry.mcps {
                if !defined_mcp_names.contains(&mcp_name.as_str()) {
                    println!("WARNING: profile template '{tpl_name}' references MCP '{mcp_name}' which is not defined in [[mcps]]");
                }
            }
        }
        // Validate skills exist on disk
        for (tpl_name, entry) in &profile.templates {
            for skill in &entry.skills {
                if !skills_dir.join(skill).is_dir() {
                    println!("WARNING: profile template '{tpl_name}' references skill '{skill}' not found at {}", skills_dir.join(skill).display());
                }
            }
        }
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

        // Resolve effective profile for this template
        let resolved = resolve_profile_entry(&profile, tpl_name);

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

        // Append context_files to AGENTS.md (batch read, single write)
        if !resolved.context_files.is_empty() {
            let agents_path = target.join("AGENTS.md");
            let profile_base_dir = profile_file
                .and_then(|p| p.parent())
                .unwrap_or(Path::new("."));

            // Read existing AGENTS.md content once
            let mut agents_content = if agents_path.exists() {
                tokio::fs::read_to_string(&agents_path).await.unwrap_or_default()
            } else {
                String::new()
            };

            let mut appended = Vec::new();
            for ctx_file in &resolved.context_files {
                let ctx_path = profile_base_dir.join(ctx_file);

                // Path traversal protection: ensure resolved path stays within profile dir
                let canonical_base = std::fs::canonicalize(profile_base_dir)
                    .unwrap_or_else(|_| profile_base_dir.to_path_buf());
                if ctx_path.exists() {
                    let canonical_ctx = std::fs::canonicalize(&ctx_path)
                        .with_context(|| format!("failed to canonicalize context path {}", ctx_path.display()))?;
                    if !canonical_ctx.starts_with(&canonical_base) {
                        println!("  WARNING: context file '{}' resolves outside profile directory, skipping", ctx_file);
                        continue;
                    }

                    let content = tokio::fs::read_to_string(&ctx_path)
                        .await
                        .with_context(|| format!("failed to read context file {}", ctx_path.display()))?;

                    agents_content.push_str("\n\n---\n\n");
                    agents_content.push_str(&content);
                    appended.push(ctx_file.as_str());
                } else {
                    println!("  WARNING: context file '{}' not found at {}", ctx_file, ctx_path.display());
                }
            }

            // Single write with all context appended
            if !appended.is_empty() {
                tokio::fs::write(&agents_path, agents_content)
                    .await
                    .with_context(|| format!("failed to write AGENTS.md with context for {tpl_name}"))?;
                println!("  context: {}", appended.join(", "));
            }
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

        // Copy skills (from templates.toml + profile)
        let mut skills: Vec<String> = config
            .templates
            .get(tpl_name.as_str())
            .map(|e| e.skills.iter().cloned().collect())
            .unwrap_or_default();

        // Merge skills from profile
        for skill in &resolved.skills {
            if !skills.contains(skill) {
                skills.push(skill.clone());
            }
        }

        if skills.is_empty() {
            println!("  (no skills)");
        } else {
            let skills_target = target.join(".opencode").join("skills");
            tokio::fs::create_dir_all(&skills_target)
                .await
                .with_context(|| format!("failed to create skills dir for {tpl_name}"))?;

            for skill in &skills {
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

        // Write mcps.json for template (merge templates.toml + profile)
        let mut mcps: Vec<String> = config
            .templates
            .get(tpl_name.as_str())
            .map(|e| e.mcps.clone())
            .unwrap_or_default();

        // Merge MCPs from profile
        for name in &resolved.mcps {
            if !mcps.contains(name) {
                mcps.push(name.clone());
            }
        }

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
        } else {
            // Remove stale mcps.json from previous deploys
            let stale = target.join(".jyc").join("mcps.json");
            if stale.exists() {
                tokio::fs::remove_file(&stale).await.ok();
            }
        }

        // Write MCP definitions from profile into .jyc/mcp-defs.json
        // Only include definitions referenced by this template's MCP names.
        let extra_defs: Vec<_> = profile.mcps.iter()
            .filter(|def| mcps.contains(&def.name))
            .collect();

        if !extra_defs.is_empty() {
            let jyc_dir = target.join(".jyc");
            tokio::fs::create_dir_all(&jyc_dir)
                .await
                .with_context(|| format!("failed to create .jyc dir for {tpl_name}"))?;
            let defs_json = serde_json::to_string_pretty(&extra_defs)?;
            tokio::fs::write(jyc_dir.join("mcp-defs.json"), defs_json)
                .await
                .with_context(|| format!("failed to write mcp-defs.json for {tpl_name}"))?;
            let def_names: Vec<_> = extra_defs.iter().map(|d| d.name.as_str()).collect();
            println!("  mcp-defs: {}", def_names.join(", "));
        } else {
            // Remove stale mcp-defs.json from previous deploys
            let stale = target.join(".jyc").join("mcp-defs.json");
            if stale.exists() {
                tokio::fs::remove_file(&stale).await.ok();
            }
        }
    }

    println!();
    println!("=== Deployment complete ===");
    println!("Templates deployed to: {}", target_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_profile_config_basic() {
        let toml_str = r#"
[[mcps]]
type = "local"
name = "ui5-mcp"
command = ["npx", "-y", "@ui5/mcp-server@latest"]

[templates.github-developer]
mcps = ["ui5-mcp"]
skills = ["sap-cap-ui5-dev"]
context_files = ["context/background.md"]
"#;
        let config: ProfileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mcps.len(), 1);
        assert_eq!(config.mcps[0].name, "ui5-mcp");
        assert_eq!(config.templates.len(), 1);

        let dev = config.templates.get("github-developer").unwrap();
        assert_eq!(dev.mcps, vec!["ui5-mcp"]);
        assert_eq!(dev.skills, vec!["sap-cap-ui5-dev"]);
        assert_eq!(dev.context_files, vec!["context/background.md"]);
    }

    #[test]
    fn test_resolve_profile_entry() {
        let toml_str = r#"
[templates.dev]
mcps = ["mcp-a", "mcp-b"]
skills = ["skill-a"]
context_files = ["ctx.md"]
"#;
        let config: ProfileConfig = toml::from_str(toml_str).unwrap();
        let resolved = resolve_profile_entry(&config, "dev");

        assert_eq!(resolved.mcps, vec!["mcp-a", "mcp-b"]);
        assert_eq!(resolved.skills, vec!["skill-a"]);
        assert_eq!(resolved.context_files, vec!["ctx.md"]);
    }

    #[test]
    fn test_resolve_profile_entry_unknown_template() {
        let toml_str = r#"
[templates.dev]
mcps = ["mcp-a"]
skills = ["skill-a"]
"#;
        let config: ProfileConfig = toml::from_str(toml_str).unwrap();
        let resolved = resolve_profile_entry(&config, "nonexistent");

        assert!(resolved.mcps.is_empty());
        assert!(resolved.skills.is_empty());
        assert!(resolved.context_files.is_empty());
    }

    #[test]
    fn test_profile_config_defaults() {
        let toml_str = r#"
[templates.minimal]
"#;
        let config: ProfileConfig = toml::from_str(toml_str).unwrap();
        let entry = config.templates.get("minimal").unwrap();

        assert!(entry.mcps.is_empty());
        assert!(entry.skills.is_empty());
        assert!(entry.context_files.is_empty());
    }
}
