mod cli;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// JYC — Channel-agnostic AI agent
#[derive(Parser)]
#[command(name = "jyc", version, about)]
struct Cli {
    /// Working directory / data root (default: platform data dir,
    /// e.g. ~/.local/share/jyc on Linux)
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
    #[command(alias = "monitor")]
    Serve(cli::serve::ServeArgs),

    /// Live TUI dashboard — connects to a running jyc serve
    Dashboard(cli::dashboard::DashboardArgs),

    /// Open a directory as an ad-hoc thread and launch chat (shortcut for `dashboard open`)
    Open {
        /// Inspect server address (also used for WebSocket chat)
        #[arg(long, default_value = "127.0.0.1:9876")]
        addr: String,

        #[command(flatten)]
        args: cli::dashboard::OpenArgs,
    },

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

    /// Manage agent templates
    Templates {
        #[command(subcommand)]
        action: cli::templates::TemplatesAction,
    },

    /// MCP reply tool server (internal — spawned by agent)
    #[command(hide = true)]
    McpReplyTool,

    /// Stop a running jyc serve process
    Stop(cli::stop::StopArgs),
}

fn init_tracing(debug: bool, verbose: bool) {
    let filter = if verbose {
        "jyc=trace,jyc_agent=trace,async_imap=debug"
    } else if debug {
        "jyc=debug,jyc_agent=debug"
    } else {
        "jyc=info,jyc_agent=info,async_imap=warn"
    };

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

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
            let expanded = jyc_utils::paths::expand_tilde(&w.to_string_lossy());
            let abs = std::fs::canonicalize(&expanded).unwrap_or(expanded);
            Ok(abs)
        }
        None => jyc_utils::paths::data_home().ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine platform data directory; pass --workdir explicitly"
            )
        }),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.debug, cli.verbose);

    let workdir = resolve_workdir(cli.workdir.as_ref())?;

    let result = match &cli.command {
        Commands::Serve(args) => cli::serve::run(args, &workdir, cli.workdir.is_some()).await,
        Commands::Dashboard(args) => match &args.command {
            Some(cli::dashboard::DashboardCommand::Open(open)) => {
                cli::dashboard::run_open(
                    &args.addr,
                    open.thread.as_deref(),
                    open.channel.as_deref(),
                    open.path.as_deref(),
                )
                .await
            }
            None => cli::dashboard::run(args, None, None).await,
        },
        Commands::Open { addr, args } => {
            cli::dashboard::run_open(
                addr,
                args.thread.as_deref(),
                args.channel.as_deref(),
                args.path.as_deref(),
            )
            .await
        }
        Commands::Config { action } => {
            cli::config::run(action, &workdir, cli.workdir.is_some()).await
        }
        Commands::Patterns { action } => {
            cli::patterns::run(action, &workdir, cli.workdir.is_some()).await
        }
        Commands::Templates { action } => cli::templates::run(action, &workdir).await,
        Commands::McpReplyTool => cli::mcp_reply::run().await,
        Commands::Stop(args) => cli::stop::run(args, &workdir).await,
    };

    if let Err(ref e) = result {
        // Log fatal error via tracing (if initialized) AND stderr (always visible)
        tracing::error!(error = %e, "Fatal error");
        eprintln!("FATAL: {e:?}");
    }

    result
}
