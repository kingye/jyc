//! `jyc agents` — list and install agent templates.
//!
//! Agent templates live in `<source>/templates/` (e.g. the jyc repo) and
//! are installed into `<target>/templates/` (default: the platform config
//! home, e.g. `~/.config/jyc/templates/`).

use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

use super::install;

/// Actions for the `agents` subcommand.
#[derive(Debug, Subcommand)]
pub enum AgentsAction {
    /// List available agents in the source directory
    List {
        /// Source directory containing templates/ (default: current directory)
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Install agents into the target directory
    Install {
        /// Agent name to install (omit to install all)
        name: Option<String>,

        /// Source directory containing templates/ (default: current directory)
        #[arg(long)]
        source: Option<PathBuf>,

        /// Target directory (default: platform config home, e.g. ~/.config/jyc)
        #[arg(long)]
        target: Option<PathBuf>,
    },
}

/// Dispatch to the appropriate subcommand handler.
pub async fn run(action: &AgentsAction) -> Result<()> {
    match action {
        AgentsAction::List { source } => {
            let source = install::resolve_source(source.as_deref())?;
            for name in install::list_entries(&source, "templates").await? {
                println!("{name}");
            }
            Ok(())
        }
        AgentsAction::Install {
            name,
            source,
            target,
        } => {
            let source = install::resolve_source(source.as_deref())?;
            let target = install::resolve_target(target.as_deref())?;
            install::install_entries(&source, &target, "templates", name.as_deref()).await
        }
    }
}
