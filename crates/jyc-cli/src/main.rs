mod cli;

use anyhow::Result;
use clap::{Parser, Subcommand, error::ErrorKind};
use std::path::{Path, PathBuf};
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

    /// Open a directory as an ad-hoc topic and launch chat (shortcut for `dashboard open`)
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
    Agents {
        #[command(subcommand)]
        action: cli::agents::AgentsAction,
    },

    /// Manage skills
    Skills {
        #[command(subcommand)]
        action: cli::skills::SkillsAction,
    },

    /// MCP reply tool server (internal — spawned by agent)
    #[command(hide = true)]
    McpReplyTool,

    /// Stop a running jyc serve process
    Stop(cli::stop::StopArgs),

    /// Manage dashboard authorization tokens
    Token(cli::token::TokenArgs),
}

/// Parse `Cli` so that bare `jyc` behaves like `jyc open` (creates an
/// ad-hoc websocket topic and opens the chat pane). When no
/// subcommand is given, we inject `open` and re-parse. With clap's
/// default `subcommand_required(true)`, a bare invocation emits
/// `DisplayHelp` (we also catch `MissingSubcommand` for safety); we
/// treat both as "no subcommand" only when `args.len() == 1` so that
/// an explicit `jyc --help` still shows help.
fn parse_cli() -> Cli {
    let args: Vec<String> = std::env::args().collect();
    match Cli::try_parse_from(&args) {
        Ok(c) => c,
        Err(e)
            if args.len() == 1
                && matches!(
                    e.kind(),
                    ErrorKind::MissingSubcommand
                        | ErrorKind::DisplayHelp
                        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) =>
        {
            let mut new_args = args;
            new_args.push("open".to_string());
            Cli::try_parse_from(new_args).unwrap_or_else(|_| e.exit())
        }
        Err(e) => e.exit(),
    }
}

fn init_tracing(debug: bool, verbose: bool, log_file: Option<&Path>) -> Result<()> {
    use anyhow::Context;

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

    if let Some(path) = log_file {
        // TUI subcommand: route logs to a file so they don't trample the
        // alternate-screen rendering. Append, create-if-missing; ensure
        // the parent dir exists so a fresh install doesn't fail.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create log directory {}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("Failed to open log file {}", path.display()))?;
        base.with_writer(file).init();
    } else if std::env::var("JOURNAL_STREAM").is_ok() {
        // Skip tracing's timestamp when running under systemd (journal adds its own)
        base.without_time().init();
    } else {
        base.init();
    }
    Ok(())
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
    let cli = parse_cli();

    // Route tracing logs to a file for subcommands that own the terminal
    // (TUI), so log writes don't break the alternate-screen render.
    // Lives in the platform data dir (next to jyc.log when workdir is the
    // default) for easy post-mortem access.
    let log_file = matches!(&cli.command, Commands::Dashboard(_) | Commands::Open { .. })
        .then(|| jyc_utils::paths::data_home().map(|h| h.join("dashboard.log")))
        .flatten();
    init_tracing(cli.debug, cli.verbose, log_file.as_deref())?;

    let workdir = resolve_workdir(cli.workdir.as_ref())?;

    let result = match &cli.command {
        Commands::Serve(args) => cli::serve::run(args, &workdir, cli.workdir.is_some()).await,
        Commands::Dashboard(args) => match &args.command {
            Some(cli::dashboard::DashboardCommand::Open(open)) => {
                cli::dashboard::run_open(
                    &args.addr,
                    &workdir,
                    open.topic.as_deref(),
                    open.channel.as_deref(),
                    open.path.as_deref(),
                    args.token.as_deref(),
                )
                .await
            }
            None => cli::dashboard::run(args, &workdir, None, None).await,
        },
        Commands::Open { addr, args } => {
            cli::dashboard::run_open(
                addr,
                &workdir,
                args.topic.as_deref(),
                args.channel.as_deref(),
                args.path.as_deref(),
                None,
            )
            .await
        }
        Commands::Config { action } => {
            cli::config::run(action, &workdir, cli.workdir.is_some()).await
        }
        Commands::Patterns { action } => {
            cli::patterns::run(action, &workdir, cli.workdir.is_some()).await
        }
        Commands::Agents { action } => cli::agents::run(action).await,
        Commands::Skills { action } => cli::skills::run(action).await,
        Commands::McpReplyTool => cli::mcp_reply::run().await,
        Commands::Stop(args) => cli::stop::run(args, &workdir).await,
        Commands::Token(args) => cli::token::run(args, &workdir),
    };

    if let Err(ref e) = result {
        // Log fatal error via tracing (if initialized) AND stderr (always visible)
        tracing::error!(error = %e, "Fatal error");
        eprintln!("FATAL: {e:?}");
    }

    result
}
