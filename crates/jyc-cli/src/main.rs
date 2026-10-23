mod cli;

use anyhow::Result;
use clap::{Parser, Subcommand, error::ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

/// JYC — Channel-agnostic AI agent
#[derive(Parser)]
#[command(name = "jyc", version, about)]
struct Cli {
    /// Data dir (default: platform data dir, e.g. ~/.local/share/jyc on Linux)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    debug: bool,

    /// Enable verbose (trace) logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Write tracing logs to a file (default) instead of stderr.
    ///
    /// Without a value: <data dir>/jyc.log.
    /// With a value: that path (e.g. /dev/stderr forces stderr).
    /// Default (no flag): <data dir>/jyc.log — the log file is named
    /// after the binary, regardless of subcommand.
    #[arg(long, global = true, num_args = 0..=1, value_name = "PATH")]
    log_file: Option<Option<PathBuf>>,

    /// Run the peripheral message pipe (`jyc-pipe`) as a child process.
    ///
    /// Pipe-only channels (feishu, github, gitee, wecom, wecom_bot, wecomkf,
    /// email) are hosted by the separate `jyc-pipe` process; without it the
    /// hub skips them and their inbound messages go unprocessed. For bare
    /// `jyc` the flag is forwarded to the auto-started `jyc serve`.
    #[arg(long, global = true)]
    pipe: bool,

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

    /// Run the dashboard against a hub on another machine, over SSH
    Connect(cli::dashboard::ConnectArgs),

    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: cli::config::ConfigAction,
    },

    /// Install a plugin archive (.tar.gz) or a plugin directory into the config dir
    Plugins {
        #[command(subcommand)]
        action: cli::plugins::PluginsAction,
    },

    /// MCP server maintenance (e.g. one-time OAuth authorization)
    Mcp {
        #[command(subcommand)]
        action: cli::mcp::McpAction,
    },

    /// Stop a running jyc serve process
    Stop(cli::stop::StopArgs),

    /// Manage dashboard authorization tokens
    Token(cli::token::TokenArgs),
}

/// Parse `Cli` so that bare `jyc` behaves like `jyc open` (creates an
/// ad-hoc websocket topic and opens the chat pane). When no subcommand is
/// given, we inject `open` and re-parse.
fn parse_cli() -> Cli {
    parse_cli_from(std::env::args().collect())
}

/// [`parse_cli`] against an explicit argv, so tests can cover the
/// bare-invocation heuristics.
fn parse_cli_from(args: Vec<String>) -> Cli {
    match Cli::try_parse_from(&args) {
        Ok(c) => c,
        Err(e) if missing_subcommand(&e, &args) => {
            let mut new_args = args;
            new_args.push("open".to_string());
            Cli::try_parse_from(new_args).unwrap_or_else(|_| e.exit())
        }
        Err(e) => e.exit(),
    }
}

/// True when the parse error means "no subcommand was given" (a bare
/// invocation, or global flags only — `jyc --pipe`) as opposed to an
/// explicit help/version request.
///
/// `MissingSubcommand` is unambiguous. The help kinds are not: a bare `jyc`
/// emits `DisplayHelp` too, but `jyc --help` must print help instead of
/// silently opening the dashboard, hence the explicit flag check.
fn missing_subcommand(e: &clap::Error, args: &[String]) -> bool {
    match e.kind() {
        ErrorKind::MissingSubcommand => true,
        ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => !args
            .iter()
            .any(|a| matches!(a.as_str(), "-h" | "--help" | "-V" | "--version")),
        _ => false,
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
        // Plain append-only file at `<path>`. No rotation: daily rotation
        // produced dated files (`jyc.log.YYYY-MM-DD`) that cluttered the
        // data dir. Use external logrotate if size-based rotation is
        // needed. `Mutex<File>` serializes writes so concurrent threads
        // don't interleave bytes mid-line.
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create log directory {}", parent.display()))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("Failed to open log file {}", path.display()))?;
        base.with_writer(Mutex::new(file)).init();
    } else if std::env::var("JOURNAL_STREAM").is_ok() {
        // Skip tracing's timestamp when running under systemd (journal adds its own)
        base.without_time().init();
    } else {
        base.init();
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_cli();

    // Log destination: the log file is named after the binary —
    // <data dir>/jyc.log for every invocation. `--log-file PATH`
    // overrides; `--log-file /dev/stderr` restores stderr.
    let log_file = match &cli.log_file {
        Some(Some(path)) => Some(path.clone()),
        Some(None) | None => jyc_utils::paths::data_dir().map(|h| h.join("jyc.log")),
    };
    init_tracing(cli.debug, cli.verbose, log_file.as_deref())?;

    let data_dir = jyc_utils::config_resolve::resolve_data_dir(cli.data_dir.as_ref())?;

    let result = match &cli.command {
        Commands::Serve(args) => {
            cli::serve::run(args, &data_dir, cli.data_dir.is_some(), cli.pipe).await
        }
        Commands::Dashboard(args) => match &args.command {
            Some(cli::dashboard::DashboardCommand::Open(open)) => {
                cli::dashboard::run_open(
                    &args.addr,
                    &data_dir,
                    open.topic.as_deref(),
                    open.channel.as_deref(),
                    open.path.as_deref(),
                    args.token.as_deref(),
                    cli.pipe,
                )
                .await
            }
            None => cli::dashboard::run(args, &data_dir, None, None, cli.pipe).await,
        },
        Commands::Open { addr, args } => {
            cli::dashboard::run_open(
                addr,
                &data_dir,
                args.topic.as_deref(),
                args.channel.as_deref(),
                args.path.as_deref(),
                None,
                cli.pipe,
            )
            .await
        }
        Commands::Connect(args) => cli::dashboard::run_connect(args, &data_dir, cli.pipe).await,
        Commands::Config { action } => {
            cli::config::run(action, &data_dir, cli.data_dir.is_some()).await
        }
        Commands::Plugins { action } => cli::plugins::run(action).await,
        Commands::Mcp { action } => cli::mcp::run(action, &data_dir, cli.data_dir.is_some()).await,
        Commands::Stop(args) => cli::stop::run(args, &data_dir).await,
        Commands::Token(args) => cli::token::run(args, &data_dir),
    };

    if let Err(ref e) = result {
        // Log fatal error via tracing (if initialized) AND stderr (always visible)
        tracing::error!(error = %e, "Fatal error");
        eprintln!("FATAL: {e:?}");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        parse_cli_from(args.iter().map(|a| a.to_string()).collect())
    }

    #[test]
    fn bare_invocation_opens_the_dashboard() {
        assert!(matches!(parse(&["jyc"]).command, Commands::Open { .. }));
    }

    /// Global flags without a subcommand must not break the bare-invocation
    /// rewrite (`jyc --pipe` used to exit with a missing-subcommand error).
    #[test]
    fn flags_without_subcommand_still_open_the_dashboard() {
        let cli = parse(&["jyc", "--pipe"]);
        assert!(cli.pipe);
        assert!(matches!(cli.command, Commands::Open { .. }));
    }

    #[test]
    fn pipe_flag_is_accepted_before_and_after_the_subcommand() {
        for args in [["jyc", "--pipe", "serve"], ["jyc", "serve", "--pipe"]] {
            let cli = parse(&args);
            assert!(cli.pipe, "{args:?}");
            assert!(matches!(cli.command, Commands::Serve(_)), "{args:?}");
        }
    }

    /// `jyc connect` takes the host positionally and needs nothing else: the
    /// remote flags default to the remote's own loopback bind and to `jyc`.
    #[test]
    fn connect_takes_a_host_and_defaults_the_rest() {
        let cli = parse(&["jyc", "connect", "devbox"]);
        match cli.command {
            Commands::Connect(args) => {
                assert_eq!(args.host, "devbox");
                assert!(!args.serve);
                assert_eq!(args.bin, "jyc");
                assert_eq!(args.remote_bind, "127.0.0.1:9876");
                assert!(args.remote_data_dir.is_none());
                assert!(args.open.topic.is_none() && args.open.path.is_none());
            }
            _ => panic!("expected `jyc connect`"),
        }
    }

    /// `-s/--serve` and the open flags ride along in one invocation.
    #[test]
    fn connect_accepts_serve_and_open_flags() {
        let cli = parse(&[
            "jyc",
            "connect",
            "dev@box",
            "-s",
            "-p",
            "~/src/app",
            "-t",
            "app",
        ]);
        match cli.command {
            Commands::Connect(args) => {
                assert!(args.serve);
                assert_eq!(args.open.path.as_deref(), Some("~/src/app"));
                assert_eq!(args.open.topic.as_deref(), Some("app"));
            }
            _ => panic!("expected `jyc connect`"),
        }
    }

    /// `jyc --help` must still print help: the help error kind is only
    /// treated as "no subcommand" when no help flag was asked for.
    #[test]
    fn explicit_help_is_not_a_missing_subcommand() {
        let args = vec!["jyc".to_string(), "--help".to_string()];
        match Cli::try_parse_from(&args) {
            Ok(_) => panic!("`jyc --help` must not parse as a command"),
            Err(err) => assert!(!missing_subcommand(&err, &args)),
        }
    }
}
