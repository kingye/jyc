//! jyc-pipe — peripheral message-pipe process.
//!
//! Hosts pipe-only channel adapters (feishu, github, gitee, wecom_bot,
//! wecom, wecomkf, email) as a separate process from the agent core
//! (`jyc`). Adapters translate platform events and forward messages to a
//! hub websocket channel over
//! the protocol documented in `docs/api.md` §3 — inbound via `message`
//! frames, replies and `topic_event` frames stream back on the same
//! connection. The pipe owns no topics, agents, or core state.
//!
//! Per the pipe process split (`docs/architecture/pipe-split.md`): every
//! configured channel whose type this process can run
//! (`jyc_types::channel::SUPPORTED_CHANNEL_TYPES`) is spawned here, and `jyc
//! serve`
//! skips exactly those in-process. No config section is involved.
//!
//! Usage: jyc-pipe [--workdir DIR] [--config FILE] [--hub WS-URL]
//!                  [--no-idle] [--reset] [-v]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

struct PipeArgs {
    workdir: Option<PathBuf>,
    config: Option<String>,
    hub: Option<String>,
    log_file: Option<Option<PathBuf>>,
    no_idle: bool,
    reset: bool,
    verbose: bool,
}

enum ParseOutcome {
    Args(PipeArgs),
    /// -h/--help requested: print usage and exit 0.
    Help,
}

const USAGE: &str = "\
jyc-pipe — peripheral message-pipe process for jyc

Runs the configured pipe-only channels (feishu, github, gitee, wecom_bot,
wecom, wecomkf, email) as a separate process, forwarding messages to the hub
over websocket. `jyc serve` skips those channels in-process; the WeCom
webhook listener (`[wecom].bind_addr`) and the email mailbox cursor
(`<workdir>/channels/<name>/.imap/`) belong to this process.

Usage: jyc-pipe [OPTIONS]

Options:
  -w, --workdir <DIR>   Working directory / data root (default: platform data dir)
  -c, --config <FILE>   Config file (default: resolved like `jyc serve`)
      --hub <WS-URL>    Hub websocket origin (default: ws://<inspect.bind>,
                        derived from the config's [inspect] section)
      --log-file [PATH] Write logs to PATH (no value: <data_home>/jyc-pipe.log);
                        default: <data_home>/jyc-pipe.log (jyc-pipe is a
                        daemon, like bare `jyc`); use /dev/stderr for stderr
      --no-idle         Use polling instead of IMAP IDLE (email channels)
      --reset           Reset monitoring state before starting (email channels)
  -v, --verbose         Verbose logging for jyc's own targets (trace; IMAP
                        protocol at debug). RUST_LOG overrides this, with the
                        wire-level third-party crates capped unless named
  -h, --help            Print this help
";

fn parse_args_from<I: Iterator<Item = String>>(it: I) -> Result<ParseOutcome> {
    let mut args = PipeArgs {
        workdir: None,
        config: None,
        hub: None,
        log_file: None,
        no_idle: false,
        reset: false,
        verbose: false,
    };
    let mut it = it.peekable();
    while let Some(arg) = it.next() {
        let mut take_value = |flag: &str| -> Result<String> {
            it.next()
                .with_context(|| format!("missing value for {flag}"))
        };
        match arg.as_str() {
            "-w" | "--workdir" => args.workdir = Some(PathBuf::from(take_value("--workdir")?)),
            "-c" | "--config" => args.config = Some(take_value("--config")?),
            "--hub" => args.hub = Some(take_value("--hub")?),
            // Optional value (clap-style `num_args = 0..=1`): consume the
            // next argument as the path unless it looks like another flag.
            "--log-file" => {
                let value = it.next_if(|a| !a.starts_with('-'));
                args.log_file = Some(value.map(PathBuf::from));
            }
            "--no-idle" => args.no_idle = true,
            "--reset" => args.reset = true,
            "-v" | "--verbose" => args.verbose = true,
            "-h" | "--help" => return Ok(ParseOutcome::Help),
            other if other.starts_with("--workdir=") => {
                args.workdir = Some(PathBuf::from(&other["--workdir=".len()..]));
            }
            other if other.starts_with("--config=") => {
                args.config = Some(other["--config=".len()..].to_string());
            }
            other if other.starts_with("--hub=") => {
                args.hub = Some(other["--hub=".len()..].to_string());
            }
            other if other.starts_with("--log-file=") => {
                args.log_file = Some(Some(PathBuf::from(&other["--log-file=".len()..])));
            }
            other => anyhow::bail!("unknown argument: {other}\n\n{USAGE}"),
        }
    }
    Ok(ParseOutcome::Args(args))
}

fn parse_args() -> Result<PipeArgs> {
    match parse_args_from(std::env::args().skip(1))? {
        ParseOutcome::Args(args) => Ok(args),
        ParseOutcome::Help => {
            print!("{USAGE}");
            std::process::exit(0);
        }
    }
}

/// Derive the hub websocket origin (`ws://<host>:<port>`, no path):
/// explicit `--hub`, else the config's `[inspect] bind` (the ws server
/// shares the inspect port). A wildcard bind maps to loopback — a
/// wildcard address is a listen-any interface, never a reachable
/// destination.
fn resolve_hub_origin(hub_arg: Option<&str>, config: &jyc_types::AppConfig) -> Result<String> {
    if let Some(url) = hub_arg {
        // Accept a full URL too — keep only scheme://authority.
        let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
        let authority = rest.split('/').next().unwrap_or(rest);
        anyhow::ensure!(!authority.is_empty(), "invalid --hub websocket URL: {url}");
        let scheme = if url.starts_with("wss://") {
            "wss"
        } else {
            "ws"
        };
        return Ok(format!("{scheme}://{authority}"));
    }
    let inspect = config.inspect.as_ref().filter(|i| i.enabled).context(
        "[inspect] is not enabled in the config and no --hub URL was given; \
         the hub websocket requires a running inspect server",
    )?;
    let bind = &inspect.bind;
    let (host, port) = bind
        .rsplit_once(':')
        .map(|(h, p)| (h, Some(p)))
        .unwrap_or((bind.as_str(), None));
    let host = match host {
        "0.0.0.0" => "127.0.0.1",
        "[::]" => "[::1]",
        h => h,
    };
    Ok(match port {
        Some(p) => format!("ws://{host}:{p}"),
        None => format!("ws://{host}"),
    })
}

/// Wire-level third-party crates that dump raw traffic at debug/trace.
///
/// This process is a websocket client of the hub, so such dumps carry frames
/// the hub generated for *other* topics: under a global `RUST_LOG` they would
/// copy other processes' content into `jyc-pipe.log` (the frame dumps of
/// `tungstenite`'s own `Received message` trace). Capped to `warn` unless the
/// operator names the target in `RUST_LOG`.
const NOISY_TARGETS: [&str; 6] = [
    "tungstenite",
    "tokio_tungstenite",
    "hyper",
    "h2",
    "reqwest",
    "mio",
];

/// Build the `EnvFilter` directives: scoped to jyc's own targets by default,
/// `RUST_LOG` when set, with the noisy wire-level crates capped.
fn filter_directives(default_filter: &str, rust_log: Option<&str>) -> String {
    let base = rust_log
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .unwrap_or(default_filter);

    let mut directives = base.to_string();
    for target in NOISY_TARGETS {
        // `target=` (rather than a bare substring) marks the operator's own
        // decision — naming `tokio_tungstenite` also leaves `tungstenite`
        // uncapped, since the inner crate is the one that reads the frames.
        if !base.contains(&format!("{target}=")) {
            directives.push_str(&format!(",{target}=warn"));
        }
    }
    directives
}

/// Log destination, mirroring `jyc`'s daemon behavior: `jyc-pipe` is a
/// long-running process with no one-shot mode, so it aligns with bare
/// `jyc` (which defaults to `jyc.log`), not with explicit `jyc`
/// subcommands (which default to stderr). Default: append to
/// `<data_home>/jyc-pipe.log`; `--log-file [PATH]` overrides, and
/// `--log-file /dev/stderr` restores stderr. Under systemd
/// (`JOURNAL_STREAM` set) stderr logs drop the timestamp — journal adds
/// its own. Plain append-only file, no rotation (same trade-off as
/// `jyc`: external logrotate if size-based rotation is needed);
/// `Mutex<File>` serializes writes across threads.
///
/// Filtering is scoped to jyc's own targets by default (same shape as
/// `jyc`'s) so third-party noise never lands in the pipe's log; see
/// [`filter_directives`] for the `RUST_LOG` precedence.
fn init_tracing(filter: &str, log_file: Option<Option<PathBuf>>) -> Result<()> {
    let base = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter_directives(
            filter,
            std::env::var("RUST_LOG").ok().as_deref(),
        )))
        .with_target(true)
        .with_thread_ids(false);

    let path = match log_file {
        Some(Some(path)) => Some(path),
        Some(None) | None => jyc_utils::paths::data_home().map(|home| home.join("jyc-pipe.log")),
    };
    if let Some(path) = path {
        let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;
        base.with_writer(std::sync::Mutex::new(file)).init();
    } else if std::env::var("JOURNAL_STREAM").is_ok() {
        base.without_time().init();
    } else {
        base.init();
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;

    // Scoped like `jyc`'s defaults: our own crates plus the IMAP protocol
    // trace (`-v`), everything else off unless `RUST_LOG` says otherwise.
    let filter = if args.verbose {
        "jyc=trace,async_imap=debug"
    } else {
        "jyc=info,async_imap=warn"
    };
    init_tracing(filter, args.log_file)?;

    let workdir = jyc_utils::config_resolve::resolve_workdir(args.workdir.as_ref())?;
    let resolution = jyc_utils::config_resolve::resolve_config(
        &workdir,
        args.config.as_deref(),
        args.workdir.is_some(),
    )?;
    let config = jyc_types::load_config_layered(
        resolution.global_config_path.as_deref(),
        &resolution.config_path,
    )?;
    let mut channel_names: Vec<&str> = config.channels.keys().map(String::as_str).collect();
    channel_names.sort_unstable();
    tracing::info!(
        config = %resolution.config_path.display(),
        channels = %channel_names.join(", "),
        "jyc-pipe loaded config"
    );

    let hub_origin = resolve_hub_origin(args.hub.as_deref(), &config)?;
    // Reuse-or-generate matches the hub's own semantics: whoever starts
    // first creates the token file, the other reuses it.
    let token = jyc_utils::auth_token::resolve_or_generate_token(&workdir)?;

    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("shutting down");
                cancel.cancel();
            }
        });
    }

    jyc_channels::pipe::run(
        Arc::new(config),
        &workdir,
        &hub_origin,
        Some(token),
        args.no_idle,
        args.reset,
        cancel,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<ParseOutcome> {
        parse_args_from(argv.iter().map(|s| s.to_string()))
    }

    #[test]
    fn log_file_flag_without_value_uses_default_path() {
        let ParseOutcome::Args(args) = parse(&["--log-file"]).unwrap() else {
            panic!("expected Args");
        };
        assert_eq!(args.log_file, Some(None));
    }

    #[test]
    fn log_file_consumes_following_value() {
        let ParseOutcome::Args(args) = parse(&["--log-file", "/tmp/pipe.log"]).unwrap() else {
            panic!("expected Args");
        };
        assert_eq!(args.log_file, Some(Some(PathBuf::from("/tmp/pipe.log"))));
    }

    #[test]
    fn log_file_equals_form() {
        let ParseOutcome::Args(args) = parse(&["--log-file=/tmp/x.log"]).unwrap() else {
            panic!("expected Args");
        };
        assert_eq!(args.log_file, Some(Some(PathBuf::from("/tmp/x.log"))));
    }

    #[test]
    fn log_file_before_another_flag_takes_no_value() {
        let ParseOutcome::Args(args) = parse(&["--log-file", "-v"]).unwrap() else {
            panic!("expected Args");
        };
        assert_eq!(args.log_file, Some(None));
        assert!(args.verbose);
    }

    #[test]
    fn test_parse_defaults() {
        let ParseOutcome::Args(args) = parse(&[]).unwrap() else {
            panic!("expected Args");
        };
        assert!(args.workdir.is_none());
        assert!(args.config.is_none());
        assert!(args.hub.is_none());
        assert!(!args.verbose);
    }

    #[test]
    fn test_parse_long_and_short_flags() {
        let ParseOutcome::Args(args) = parse(&[
            "--workdir",
            "/data",
            "-c",
            "custom.toml",
            "--hub",
            "ws://127.0.0.1:9876",
            "-v",
        ])
        .unwrap() else {
            panic!("expected Args");
        };
        assert_eq!(args.workdir, Some(PathBuf::from("/data")));
        assert_eq!(args.config.as_deref(), Some("custom.toml"));
        assert_eq!(args.hub.as_deref(), Some("ws://127.0.0.1:9876"));
        assert!(args.verbose);
    }

    #[test]
    fn test_parse_equals_form_and_help() {
        let ParseOutcome::Args(args) =
            parse(&["--hub=ws://h:9876/ws/agents", "--config=x.toml"]).unwrap()
        else {
            panic!("expected Args");
        };
        assert_eq!(args.hub.as_deref(), Some("ws://h:9876/ws/agents"));
        assert_eq!(args.config.as_deref(), Some("x.toml"));
        assert!(matches!(parse(&["--help"]).unwrap(), ParseOutcome::Help));
    }

    #[test]
    fn test_parse_missing_value_and_unknown_flag_err() {
        assert!(parse(&["--hub"]).is_err());
        assert!(parse(&["--nope"]).is_err());
    }

    fn config_from(toml: &str) -> jyc_types::AppConfig {
        jyc_types::load_config_from_str(toml).expect("test config should parse")
    }

    #[test]
    fn test_resolve_hub_origin_explicit_wins_and_strips_path() {
        let config = config_from("[ai]");
        assert_eq!(
            resolve_hub_origin(Some("ws://x:9876/ws/adhoc"), &config).unwrap(),
            "ws://x:9876"
        );
        assert_eq!(
            resolve_hub_origin(Some("ws://x:9876"), &config).unwrap(),
            "ws://x:9876"
        );
    }

    #[test]
    fn test_resolve_hub_origin_derives_from_inspect_bind() {
        let config = config_from("[ai]\n[inspect]\nenabled = true\nbind = \"0.0.0.0:9876\"\n");
        assert_eq!(
            resolve_hub_origin(None, &config).unwrap(),
            "ws://127.0.0.1:9876"
        );
        let ipv6 = config_from("[ai]\n[inspect]\nenabled = true\nbind = \"[::]:9876\"\n");
        assert_eq!(resolve_hub_origin(None, &ipv6).unwrap(), "ws://[::1]:9876");
    }

    #[test]
    fn test_resolve_hub_origin_requires_inspect() {
        let config = config_from("[ai]");
        assert!(resolve_hub_origin(None, &config).is_err());
        let disabled = config_from("[ai]\n[inspect]\nenabled = false\n");
        assert!(resolve_hub_origin(None, &disabled).is_err());
    }

    #[test]
    fn filter_keeps_the_default_and_caps_wire_crates() {
        let raw = filter_directives("jyc=info,async_imap=warn", None);
        let directives: Vec<&str> = raw.split(',').collect();
        assert_eq!(directives[0], "jyc=info");
        assert_eq!(directives[1], "async_imap=warn");
        assert!(directives.contains(&"tungstenite=warn"));
        assert!(directives.contains(&"hyper=warn"));
    }

    #[test]
    fn filter_honours_rust_log_but_still_caps_wire_crates() {
        // A global `RUST_LOG=trace` must not copy hub-generated frames into
        // the pipe's log.
        let raw = filter_directives("jyc=info", Some("trace"));
        let directives: Vec<&str> = raw.split(',').collect();
        assert_eq!(directives[0], "trace");
        assert!(directives.contains(&"tungstenite=warn"));
    }

    #[test]
    fn filter_respects_an_explicitly_named_wire_crate() {
        let raw = filter_directives("jyc=info", Some("trace,tungstenite=trace"));
        let directives: Vec<&str> = raw.split(',').collect();
        assert!(directives.contains(&"tungstenite=trace"));
        assert!(!directives.contains(&"tungstenite=warn"));
        assert!(directives.contains(&"hyper=warn"));
    }
}
