//! `jyc skills` — list and install skills.
//!
//! Skills live in `<source>/skills/` (e.g. the jyc repo) and are installed
//! into `<target>/skills/` (default: the platform config home, e.g.
//! `~/.config/jyc/skills/`).

use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

use super::install;

/// Actions for the `skills` subcommand.
#[derive(Debug, Subcommand)]
pub enum SkillsAction {
    /// List available skills in the source directory
    List {
        /// Source directory containing skills/ (default: current directory)
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Install skills into the target directory
    Install {
        /// Skill name to install (omit to install all)
        name: Option<String>,

        /// Source directory containing skills/ (default: current directory)
        #[arg(long)]
        source: Option<PathBuf>,

        /// Target directory (default: platform config home, e.g. ~/.config/jyc)
        #[arg(long)]
        target: Option<PathBuf>,
    },
}

/// Dispatch to the appropriate subcommand handler.
pub async fn run(action: &SkillsAction) -> Result<()> {
    match action {
        SkillsAction::List { source } => {
            let source = install::resolve_source(source.as_deref())?;
            for name in install::list_entries(&source, "skills").await? {
                println!("{name}");
            }
            Ok(())
        }
        SkillsAction::Install {
            name,
            source,
            target,
        } => {
            let source = install::resolve_source(source.as_deref())?;
            let target = install::resolve_target(target.as_deref())?;
            install::install_entries(&source, &target, "skills", name.as_deref()).await
        }
    }
}
