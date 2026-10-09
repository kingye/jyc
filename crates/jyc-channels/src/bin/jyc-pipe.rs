//! jyc-pipe — peripheral message-pipe process.
//!
//! Hosts pipe-only channel adapters (feishu, wecom, github, ...) as a
//! separate process from the agent core (`jyc`). Adapters translate
//! platform events and forward messages to a hub websocket channel over
//! the protocol documented in `docs/api.md` §3 — inbound via `message`
//! frames, replies and `topic_event` frames stream back on the same
//! connection. The pipe owns no topics, agents, or core state.
//!
//! Step 2 of the pipe process split (`docs/architecture/pipe-split.md`):
//! this binary connects to the hub, authenticates with the inspect auth
//! token, and logs the received frame stream. Adapter wiring arrives in
//! step 3 (feishu first).
//!
//! Usage: jyc-pipe [--workdir DIR] [--config FILE] [--hub WS-URL] [-v]

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

/// Hub channel a pipe connects to by default when deriving the URL from
/// `[inspect] bind`.
const DEFAULT_HUB_CHANNEL: &str = "agents";
/// Reconnect backoff ceiling.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

struct PipeArgs {
    workdir: Option<PathBuf>,
    config: Option<String>,
    hub: Option<String>,
    verbose: bool,
}

const USAGE: &str = "\
jyc-pipe — peripheral message-pipe process for jyc

Usage: jyc-pipe [OPTIONS]

Options:
  -w, --workdir <DIR>   Working directory / data root (default: platform data dir)
  -c, --config <FILE>   Config file (default: resolved like `jyc serve`)
      --hub <WS-URL>    Hub websocket URL (default: ws://<inspect.bind>/ws/agents,
                        derived from the config's [inspect] section)
  -v, --verbose         Enable debug logging
  -h, --help            Print this help
";

fn parse_args() -> Result<PipeArgs> {
    let mut args = PipeArgs {
        workdir: None,
        config: None,
        hub: None,
        verbose: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut take_value = |flag: &str| -> Result<String> {
            it.next()
                .with_context(|| format!("missing value for {flag}"))
        };
        match arg.as_str() {
            "-w" | "--workdir" => args.workdir = Some(PathBuf::from(take_value("--workdir")?)),
            "-c" | "--config" => args.config = Some(take_value("--config")?),
            "--hub" => args.hub = Some(take_value("--hub")?),
            "-v" | "--verbose" => args.verbose = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other if other.starts_with("--workdir=") => {
                args.workdir = Some(PathBuf::from(&other["--workdir=".len()..]));
            }
            other if other.starts_with("--config=") => {
                args.config = Some(other["--config=".len()..].to_string());
            }
            other if other.starts_with("--hub=") => {
                args.hub = Some(other["--hub=".len()..].to_string());
            }
            other => anyhow::bail!("unknown argument: {other}\n\n{USAGE}"),
        }
    }
    Ok(args)
}

/// Derive the hub websocket URL: explicit `--hub`, else the config's
/// `[inspect] bind` (the ws server shares the inspect port).
fn resolve_hub_url(hub_arg: Option<&str>, config: &jyc_types::AppConfig) -> Result<String> {
    if let Some(url) = hub_arg {
        return Ok(url.to_string());
    }
    let inspect = config.inspect.as_ref().filter(|i| i.enabled).context(
        "[inspect] is not enabled in the config and no --hub URL was given; \
             the hub websocket requires a running inspect server",
    )?;
    Ok(format!("ws://{}/ws/{}", inspect.bind, DEFAULT_HUB_CHANNEL))
}

/// Log a received hub frame at a level matching its type.
fn log_frame(text: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        tracing::debug!(frame = %text, "hub frame (unparseable)");
        return;
    };
    let frame_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("?");
    let topic = v.get("topic").and_then(|t| t.as_str()).unwrap_or("");
    match frame_type {
        "reply" => {
            let preview: String = v
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            tracing::info!(topic, text = %preview, "hub reply");
        }
        "topic_event" => {
            // TopicEvent serializes as an externally-tagged enum: the
            // single key of `event` is the variant name.
            let variant = v
                .get("event")
                .and_then(|e| e.as_object())
                .and_then(|o| o.keys().next())
                .map(String::as_str)
                .unwrap_or("?");
            tracing::debug!(topic, event = variant, "hub topic_event");
        }
        other => tracing::trace!(topic, r#type = other, "hub frame"),
    }
}

/// One connection lifetime: connect, then read frames until the server
/// closes the connection or cancellation fires. Protocol-level pings are
/// answered automatically by tungstenite while reading.
async fn run_connection(hub_url: &str, token: &str, cancel: CancellationToken) -> Result<()> {
    let mut request = hub_url
        .into_client_request()
        .context("invalid --hub websocket URL")?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}")
            .parse()
            .context("invalid auth token header value")?,
    );
    let (stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .context("websocket connect failed")?;
    tracing::info!(hub = %hub_url, "jyc-pipe connected to hub");

    let mut stream = stream;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            msg = stream.next() => {
                match msg {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        log_frame(&text);
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                        tracing::info!("hub closed the connection");
                        return Ok(());
                    }
                    Some(Ok(_)) => {} // ping/pong/binary: handled by tungstenite
                    Some(Err(e)) => return Err(e).context("websocket read failed"),
                    None => return Ok(()), // stream ended
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;

    let filter = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .init();

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
    let enabled_channels = channel_names.join(", ");
    tracing::info!(
        config = %resolution.config_path.display(),
        channels = %enabled_channels,
        "jyc-pipe loaded config"
    );

    let hub_url = resolve_hub_url(args.hub.as_deref(), &config)?;
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

    // Reconnect loop: backoff doubles per failed/clean-lost connection,
    // reset on a successful connect.
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_connection(&hub_url, &token, cancel.clone()).await {
            Ok(()) if cancel.is_cancelled() => break,
            Ok(()) => {
                tracing::info!("reconnecting to hub");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(error = %e, "hub connection lost; reconnecting");
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
    Ok(())
}
