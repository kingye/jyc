mod channels;
mod cli;
mod config;
mod core;
mod mcp;
mod security;
mod services;
mod utils;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// JYC — Channel-agnostic AI agent
#[derive(Parser)]
#[command(name = "jyc", version, about)]
struct Cli {
    /// Working directory (default: current directory)
    #[arg(short, long, global = true)]
    workdir: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    debug: bool,

    /// Enable verbose (trace) logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Monitor inbound channels and process messages with AI
    Monitor(cli::monitor::MonitorArgs),

    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: cli::config::ConfigAction,
    },

    /// Manage message patterns
    Patterns {
        #[command(subcommand)]
        action: cli::patterns::PatternsAction,
    },

    /// Show current monitoring state
    State,

    /// MCP reply tool server (internal — spawned by OpenCode)
    #[command(hide = true)]
    McpReplyTool,

    /// MCP vision tool server (internal — spawned by OpenCode)
    #[command(hide = true)]
    McpVisionTool,
}

fn init_tracing(debug: bool, verbose: bool) {
    let filter = if verbose {
        "jyc=trace,async_imap=debug"
    } else if debug {
        "jyc=debug"
    } else {
        "jyc=info,async_imap=warn"
    };

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(filter));

    let base = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_thread_ids(false);

    // Skip tracing's timestamp when running under systemd (journal adds its own)
    if std::env::var("JOURNAL_STREAM").is_ok() {
        base.without_time().init();
    } else {
        base.init();
    }
}

fn resolve_workdir(workdir: Option<&PathBuf>) -> Result<PathBuf> {
    match workdir {
        Some(w) => {
            let expanded = if w.starts_with("~") {
                if let Some(home) = dirs_home() {
                    home.join(w.strip_prefix("~").unwrap())
                } else {
                    w.clone()
                }
            } else {
                w.clone()
            };
            let abs = std::fs::canonicalize(&expanded).unwrap_or(expanded);
            Ok(abs)
        }
        None => Ok(std::env::current_dir()?),
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.debug, cli.verbose);

    let workdir = resolve_workdir(cli.workdir.as_ref())?;

    match &cli.command {
        Commands::Monitor(args) => {
            cli::monitor::run(args, &workdir).await
        }
        Commands::Config { action } => {
            cli::config::run(action, &workdir).await
        }
        Commands::Patterns { action } => {
            cli::patterns::run(action, &workdir).await
        }
        Commands::State => {
            cli::state::run(&workdir).await
        }
        Commands::McpReplyTool => {
            cli::mcp_reply::run().await
        }
        Commands::McpVisionTool => {
            cli::mcp_vision::run().await
        }
    }
}
