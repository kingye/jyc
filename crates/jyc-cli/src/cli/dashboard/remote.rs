//! `jyc connect <host>` — the hub runs on another machine, the dashboard runs
//! here.
//!
//! The transport is an `ssh -N -L` child process: the dashboard keeps speaking
//! plain HTTP/WS to `127.0.0.1:<local port>`, and ssh forwards that to the
//! remote's own inspect bind. Nothing is exposed on the remote beyond the
//! loopback bind it already had, the tunnel is loopback on both ends (so no
//! TLS is needed), and the SSH config, keys, agent and jump hosts the user
//! already has apply unchanged.

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use jyc_inspect::client::InspectClient;
use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

use super::OpenArgs;
use crate::cli::resolve::normalize_inspect_client_addr;

/// How long the local end of the tunnel may take to accept connections.
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// First delay before reopening a dead ssh tunnel.
const TUNNEL_RETRY_MIN: Duration = Duration::from_secs(1);
/// Longest backoff added after a failed reopen attempt — the same cadence as
/// the dashboard's own WebSocket reconnect. An attempt costs its own setup
/// time on top (up to [`TUNNEL_READY_TIMEOUT`] while ssh hangs).
const TUNNEL_RETRY_MAX: Duration = Duration::from_secs(30);
/// A tunnel that dies sooner than this never had a chance to serve; its death
/// keeps the backoff growing instead of resetting it.
const TUNNEL_MIN_UPTIME: Duration = Duration::from_secs(3);
/// How long a remote `jyc serve` we started may take to answer.
const SERVE_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one ssh command may take, end to end.
///
/// `ConnectTimeout` bounds only the TCP connect: name resolution, a hanging
/// `ProxyCommand`/`ProxyJump`, an sshd that accepts and never sends a banner,
/// and a remote command that never exits are all unbounded there. Every
/// bootstrap step goes through [`Remote::capture`], and all of them run before
/// the TUI takes the terminal, so a hang has no way out but this deadline.
const SSH_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long fetching the remote log tail may take on the failure path.
///
/// Shorter than [`SSH_COMMAND_TIMEOUT`] on purpose: when ssh itself is what
/// failed, the diagnosis must not wait another half minute to be printed.
const LOG_TAIL_TIMEOUT: Duration = Duration::from_secs(5);

/// Arguments for `jyc connect`.
#[derive(Args, Debug)]
pub struct ConnectArgs {
    /// SSH target: `[user@]host`, or an alias from ~/.ssh/config
    pub host: String,

    /// Start `jyc serve` on the remote host when it is not running
    #[arg(short = 's', long)]
    pub serve: bool,

    /// jyc executable on the remote host
    #[arg(long, default_value = "jyc")]
    pub bin: String,

    /// Data dir on the remote host (default: the remote's own)
    #[arg(long)]
    pub remote_data_dir: Option<String>,

    /// Inspect server address on the remote host (default: read from the remote config)
    #[arg(long)]
    pub remote_bind: Option<String>,

    /// Token of the remote hub (default: read on the remote host)
    ///
    /// Deliberately not tied to `$JYC_DASHBOARD_TOKEN`: that variable belongs
    /// to a local dashboard, and sending it to another machine would fail as a
    /// 401 with nothing pointing at the cause.
    #[arg(long)]
    pub token: Option<String>,

    /// Extra ssh option, `KEY=VALUE` (repeatable), e.g. `--ssh-opt Compression=no`
    #[arg(long = "ssh-opt", value_name = "KEY=VALUE")]
    pub ssh_opts: Vec<String>,

    /// Open a directory on the remote host as a topic and go straight to chat
    #[command(flatten)]
    pub open: OpenArgs,
}

/// The remote end of a `jyc connect` session.
pub(super) struct Remote {
    host: String,
    bin: String,
    data_dir: Option<String>,
    /// Effective `[inspect].bind` on the remote. Kept verbatim for messages and
    /// link rewriting; wildcard hosts are not usable link hosts either.
    bind: String,
    /// Connectable tunnel target: a wildcard bind host replaced with loopback.
    tunnel_bind: String,
    /// The remote jyc predates `config inspect-bind`, so `bind` is the default.
    bind_fallback: bool,
    /// Extra `-o KEY=VALUE` options for every ssh invocation.
    ssh_opts: Vec<String>,
}

impl Remote {
    pub(super) async fn from_args(args: &ConnectArgs) -> Result<Self> {
        let probe = Self {
            host: args.host.clone(),
            bin: args.bin.clone(),
            data_dir: args.remote_data_dir.clone(),
            bind: jyc_types::DEFAULT_INSPECT_BIND.to_string(),
            tunnel_bind: jyc_types::DEFAULT_INSPECT_BIND.to_string(),
            bind_fallback: false,
            ssh_opts: args.ssh_opts.clone(),
        };
        let (bind, bind_fallback) = match &args.remote_bind {
            Some(bind) => (bind.clone(), false),
            None => probe.discover_bind().await?,
        };
        Ok(Self {
            tunnel_bind: normalize_inspect_client_addr(&bind),
            bind,
            bind_fallback,
            ..probe
        })
    }

    /// Ask the remote jyc for its effective inspect bind.
    ///
    /// The query runs on the remote because config layering belongs to that
    /// install (`--remote-data-dir`, its platform config dir, and defaults).
    /// An old jyc without `config inspect-bind` falls back to the historical
    /// default so default-port remotes keep working across versions.
    async fn discover_bind(&self) -> Result<(String, bool)> {
        let query = command_line(&self.argv(&["config", "inspect-bind"]));
        match self.capture(&query).await {
            Ok(output) => Ok((parse_inspect_bind_output(&output)?, false)),
            Err(err) if remote_lacks_inspect_bind(&err) => {
                tracing::warn!(
                    host = %self.host,
                    error = %err,
                    "remote jyc does not support `config inspect-bind`; using the default remote bind"
                );
                Ok((jyc_types::DEFAULT_INSPECT_BIND.to_string(), true))
            }
            Err(err) => Err(err).context(
                "could not query the remote inspect bind with `jyc config inspect-bind`; \
                 pass --remote-bind explicitly or upgrade the remote jyc",
            ),
        }
    }

    /// The host:port the remote inspect server listens on, split.
    ///
    /// Bracketed IPv6 is accepted (`[::1]:9876`); a bare v6 literal is not,
    /// because `host:port` then has no unambiguous port.
    fn bind_parts(&self) -> Result<(&str, u16)> {
        split_bind_host_port(&self.bind)
            .map_err(|err| anyhow!("invalid --remote-bind '{}': {err}", self.bind))
    }

    /// The port the remote inspect server listens on — the port the tunnel
    /// forwards to.
    pub(super) fn bind_port(&self) -> Result<u16> {
        Ok(self.bind_parts()?.1)
    }

    /// The host part of [`Remote::bind`], which is what the links the server
    /// generates start with (unless `[inspect] base_url` overrides them).
    pub(super) fn bind_host(&self) -> Result<&str> {
        Ok(self.bind_parts()?.0)
    }

    /// One ssh invocation running `remote_cmd` on the host.
    ///
    /// `BatchMode=yes` is deliberate: both bootstrap and tunnel happen before
    /// the TUI takes the terminal, and a password prompt nobody can see is a
    /// hang. Key/agent auth is the documented requirement, and the error text
    /// says how to check it.
    fn ssh(&self, remote_cmd: &str) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .args(self.ssh_opt_args())
            .arg(&self.host)
            .arg(remote_cmd);
        cmd
    }

    /// `--ssh-opt` values as `-o KEY=VALUE` pairs.
    ///
    /// Appended after the defaults everywhere, so a user option wins over one
    /// this module sets: ssh honours the last `-o` for a given key.
    fn ssh_opt_args(&self) -> Vec<OsString> {
        self.ssh_opts
            .iter()
            .flat_map(|opt| [OsString::from("-o"), OsString::from(opt)])
            .collect()
    }

    /// Run a command over ssh and return its trimmed stdout.
    async fn capture(&self, remote_cmd: &str) -> Result<String> {
        let mut cmd = self.ssh(remote_cmd);
        // Dropping the future on timeout must not leave the ssh child behind.
        cmd.stdin(Stdio::null()).kill_on_drop(true);
        let out = tokio::time::timeout(SSH_COMMAND_TIMEOUT, cmd.output())
            .await
            .with_context(|| {
                format!(
                    "ssh {} did not answer within {}s",
                    self.host,
                    SSH_COMMAND_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("failed to run `ssh {}`", self.host))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("`ssh {} {remote_cmd}` failed: {}", self.host, stderr.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// The remote server's own arguments for one jyc subcommand.
    fn argv(&self, subcommand: &[&str]) -> Vec<String> {
        let mut argv = vec![self.bin.clone()];
        if let Some(dir) = &self.data_dir {
            argv.push("--data-dir".to_string());
            argv.push(dir.clone());
        }
        argv.extend(subcommand.iter().map(|s| s.to_string()));
        argv
    }

    /// The dashboard token the remote server accepts, read from its own data
    /// dir. Not cached anywhere: it is the remote install's secret, and a copy
    /// on this machine would be a second place to keep in sync (and to leak).
    pub(super) async fn token(&self) -> Result<String> {
        // No context here: every caller says what it wanted the token for, and
        // a second "could not read the dashboard token" above it only repeats.
        self.capture(&command_line(&self.argv(&["token", "show"])))
            .await
    }

    /// The remote user's home directory — the default topic path, and what
    /// `~` in `-p` expands to.
    pub(super) async fn home(&self) -> Result<String> {
        self.capture(r#"printf '%s' "$HOME""#)
            .await
            .context("could not read $HOME on the remote host")
    }

    /// Start `jyc serve` on the remote host and return at once.
    ///
    /// `nohup` is the part that keeps it alive: it ignores SIGHUP, so closing
    /// the ssh session — which hangs up the remote shell and its jobs — does
    /// not take the hub down. `setsid` is added where it exists, to also drop
    /// the controlling terminal; it is optional on purpose, since macOS ships
    /// no `setsid` and `nohup` plus redirected stdio is enough there.
    pub(super) async fn start_serve(&self, pipe: bool) -> Result<()> {
        let mut argv = self.argv(&["serve"]);
        if pipe {
            argv.push("--pipe".to_string());
        }
        let serve = command_line(&argv);
        // All three stdio channels are cut loose: the channel ssh forwards
        // closes when the shell exits only if nothing it left behind still
        // holds them, and `serve --pipe` would otherwise hand them to its
        // `jyc-pipe` child.
        let cmd = format!(
            "if command -v setsid >/dev/null 2>&1; then setsid nohup {serve} >/dev/null 2>&1 </dev/null & \
             else nohup {serve} >/dev/null 2>&1 </dev/null & fi"
        );
        self.capture(&cmd)
            .await
            .context("could not start `jyc serve` on the remote host")?;
        Ok(())
    }

    /// Last lines of the remote server log, for a failed start.
    ///
    /// Empty without `--remote-data-dir`: the log lives under the remote's
    /// default data dir, which only the remote knows how to name.
    async fn log_tail(&self) -> String {
        let Some(dir) = &self.data_dir else {
            return String::new();
        };
        let log = format!("{}/jyc.log", dir.trim_end_matches('/'));
        self.capture(&format!("tail -n 20 {}", shell_quote(&log)))
            .await
            .unwrap_or_default()
    }
}

/// First line of `config inspect-bind` output is the effective bind.
fn parse_inspect_bind_output(output: &str) -> Result<String> {
    let bind = output
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .context("remote `jyc config inspect-bind` returned no bind address")?;
    // Validate here, not in `bind_parts`: a malformed remote `[inspect].bind`
    // would otherwise surface as "invalid --remote-bind", a flag the user
    // never passed.
    split_bind_host_port(bind).map_err(|err| {
        anyhow!(
            "remote `jyc config inspect-bind` returned an invalid bind '{bind}': {err}. \
             Check [inspect].bind in the remote's config."
        )
    })?;
    Ok(bind.to_string())
}

/// Split a `host:port` bind string, bracketed IPv6 accepted.
///
/// Shared by the `--remote-bind` flag parser and the remote
/// `config inspect-bind` output parser; each caller words the error for its
/// own source.
fn split_bind_host_port(bind: &str) -> Result<(&str, u16)> {
    let invalid = || format!("expected host:port, got '{bind}'");
    let (host, port) = bind.rsplit_once(':').with_context(invalid)?;
    if host.is_empty() {
        bail!("missing host in '{bind}'");
    }
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        bail!("bracket IPv6 literals, e.g. '[::1]:9876'");
    }
    let port = port.parse::<u16>().with_context(invalid)?;
    Ok((host, port))
}

/// Whether a remote command failure is an old jyc not knowing the subcommand.
fn remote_lacks_inspect_bind(err: &anyhow::Error) -> bool {
    let text = err.to_string();
    text.contains("unrecognized subcommand") || text.contains("unexpected argument")
}

/// The shell line for an argv, every argument quoted.
fn command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote one argument for a POSIX shell.
pub(super) fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// The ssh invocation that carries the forward.
///
/// `-C` (compression) is on by default, and it is the one option here that is
/// about throughput rather than survival: the dashboard's traffic is JSON that
/// barely changes between frames — above all `Thinking` events, which re-send
/// the whole reasoning every 500 ms — and zlib collapses such a frame to a few
/// dozen bytes because it is a prefix of the one before it. On a thin uplink
/// that is the difference between a usable session and a smeared one; it costs
/// CPU on both ends and `--ssh-opt Compression=no` turns it off.
fn tunnel_args(remote: &Remote, local_port: u16) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-N".into(),
        "-C".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        // A local port we cannot bind is a failure worth reporting, not a
        // silent "tunnel that forwards nothing".
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        // Let ssh notice a dead peer instead of forwarding into a black
        // hole for as long as the dashboard lives.
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    args.extend(remote.ssh_opt_args());
    args.push("-L".into());
    // The forward targets the effective remote bind. A configured wildcard is
    // represented here by loopback (the connectable form of that listener).
    args.push(format!("127.0.0.1:{local_port}:{}", remote.tunnel_bind).into());
    args.push(remote.host.clone().into());
    args
}

/// A live `ssh -N -L` forward.
///
/// Dropping it ends the tunnel; the remote hub keeps running — it may have
/// other dashboards and channels using it.
pub(super) struct Tunnel {
    /// `127.0.0.1:<port>` — what the dashboard's HTTP and WS clients talk to.
    pub(super) addr: String,
    /// `(from, to)` when the local port differs from the remote one: publish
    /// links carry the remote's own `http://127.0.0.1:<port>`, which does not
    /// resolve here, so message text is translated on the way in.
    pub(super) rewrite: Option<(String, String)>,
    /// Held for its `kill_on_drop`; reaped by [`Tunnel::wait`] under the
    /// supervisor.
    child: Child,
}

/// The error for an ssh child that exited before its listener was ready: its
/// own stderr is the diagnosis.
async fn tunnel_exit_error(
    host: &str,
    status: std::process::ExitStatus,
    child: &mut Child,
) -> anyhow::Error {
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr).await;
    }
    anyhow::anyhow!(
        "ssh tunnel to {host} exited ({status}): {}\n  \
         the tunnel needs non-interactive auth — check `ssh {host} true` and your agent",
        stderr.trim(),
    )
}

impl Tunnel {
    /// Open the forward and wait until ssh accepts connections on it.
    pub(super) async fn open(remote: &Remote, local_port: u16, remote_port: u16) -> Result<Self> {
        let addr = format!("127.0.0.1:{local_port}");
        // Refuse to even spawn when something else owns the port: ssh would
        // die of its bind failure (`ExitOnForwardFailure`) and the readiness
        // probe below could then connect to that foreign listener and declare
        // the wrong tunnel up.
        if port_has_listener(local_port) {
            bail!(
                "local port {local_port} is already in use by another listener; cannot open the tunnel there"
            );
        }
        let args = tunnel_args(remote, local_port);

        let mut child = Command::new("ssh")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to run `ssh {}`", remote.host))?;

        // ssh binds the local listener as soon as it has authenticated, so a
        // successful connect is the readiness signal. A run that dies first
        // (unknown host, refused local port) reports its own stderr instead.
        let deadline = tokio::time::Instant::now() + TUNNEL_READY_TIMEOUT;
        loop {
            if TcpStream::connect(&addr).await.is_ok() {
                // The probe proves *a* listener exists — ours, or a foreign
                // one. Only count it while our ssh is still alive; a bind
                // failure exits ssh within the first iteration.
                if let Some(status) = child.try_wait()? {
                    return Err(tunnel_exit_error(&remote.host, status, &mut child).await);
                }
                break;
            }
            if let Some(status) = child.try_wait()? {
                return Err(tunnel_exit_error(&remote.host, status, &mut child).await);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "ssh tunnel to {} did not come up within {}s",
                    remote.host,
                    TUNNEL_READY_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // stderr was piped so a failed start can report it above; afterwards
        // nothing reads it, and a chatty ssh could fill the 64 KB pipe and
        // block on write. Drain it for the rest of the tunnel's life.
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut sink = tokio::io::sink();
                let _ = tokio::io::copy(&mut stderr, &mut sink).await;
            });
        }

        let rewrite = rewrite_pair(local_port, remote_port, remote.bind_host()?);
        Ok(Self {
            addr,
            rewrite,
            child,
        })
    }

    /// Wait for the ssh child to exit, whatever the status.
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
}

/// Delay after a failed reopen attempt: 1s, 2s, … capped at [`TUNNEL_RETRY_MAX`].
fn next_tunnel_backoff(prev: Duration) -> Duration {
    (prev * 2).min(TUNNEL_RETRY_MAX)
}

/// Backoff to use once a tunnel has died: one that demonstrably served resets
/// to the minimum; one that died on arrival keeps the cadence growing, so a
/// zero-progress respawn loop cannot churn at full rate.
fn next_backoff_after_death(backoff: Duration, uptime: Duration) -> Duration {
    if uptime >= TUNNEL_MIN_UPTIME {
        TUNNEL_RETRY_MIN
    } else {
        next_tunnel_backoff(backoff)
    }
}

/// Keep the dashboard's route to the hub alive.
///
/// ssh exits on its own once its `ServerAlive*` watchdog concludes the peer is
/// gone (~45s of outage), and the dashboard's WS client retries forever — but
/// only against a listener that exists. Without this supervisor a long outage
/// leaves the UI reconnecting into a dead local port no matter how long it
/// waits. So: wait out each ssh death, then reopen the same local port (the
/// dashboard's addr never changes) with backoff until the network is back.
///
/// Only the transport is revived: a remote host reboot also kills `jyc serve`
/// there, and this does not restart it — the UI keeps reconnecting into a
/// silent hub until `jyc connect --serve` is run again.
///
/// Runs until aborted. `Tunnel`'s `kill_on_drop` ends the forward on the way
/// out, exactly as an unsupervised tunnel did when the dashboard exited.
pub(super) async fn supervise_tunnel(
    remote: Remote,
    local_port: u16,
    remote_port: u16,
    tunnel: Tunnel,
) -> ! {
    supervise_tunnel_loop(tunnel, || Tunnel::open(&remote, local_port, remote_port)).await
}

async fn supervise_tunnel_loop<F, Fut>(mut tunnel: Tunnel, mut open: F) -> !
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Tunnel>>,
{
    let mut backoff = TUNNEL_RETRY_MIN;
    loop {
        let opened_at = std::time::Instant::now();
        match tunnel.wait().await {
            Ok(status) => tracing::warn!(
                addr = %tunnel.addr,
                status = %status,
                "ssh tunnel exited; reopening"
            ),
            Err(err) => {
                tracing::warn!(
                    addr = %tunnel.addr,
                    error = %err,
                    "waiting on the ssh tunnel failed; killing it and reopening"
                );
                // A failed wait did not reap the child, so the forward may
                // still be alive and still own the port — kill it before
                // reopening, or the occupancy check in `open` would refuse
                // forever with a misleading message.
                if let Err(kill_err) = tunnel.child.kill().await {
                    tracing::warn!(error = %kill_err, "failed to kill the ssh tunnel child");
                }
            }
        }
        // The death moves the backoff, so the uptime is the *old* tunnel's
        // alone: sample it here, before reopening adds its own duration on
        // top of the measurement.
        let uptime = opened_at.elapsed();
        loop {
            match open().await {
                Ok(next) => {
                    tunnel = next;
                    break;
                }
                Err(err) => {
                    tracing::warn!(
                        addr = %tunnel.addr,
                        error = %err,
                        backoff_ms = backoff.as_millis() as u64,
                        "ssh tunnel reopen failed; retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = next_tunnel_backoff(backoff);
                }
            }
        }
        backoff = next_backoff_after_death(backoff, uptime);
    }
}

/// The message-text translation a tunnel needs, if any: publish links carry
/// the remote's own URL, which only resolves here when the ports happen to
/// match.
///
/// The host comes from `--remote-bind` because that is what the server builds
/// links from when `[inspect] base_url` is unset. Two cases are outside this
/// rewrite's reach, and the docs say so: a wildcard bind (the server swaps in
/// its LAN IP, which this machine cannot know) and a configured `base_url`
/// (which may be anything at all). Same-port preference is all that is left
/// there.
fn rewrite_pair(local_port: u16, remote_port: u16, bind_host: &str) -> Option<(String, String)> {
    if local_port == remote_port || is_wildcard(bind_host) {
        return None;
    }
    Some((
        format!("http://{bind_host}:{remote_port}"),
        format!("http://127.0.0.1:{local_port}"),
    ))
}

/// Whether `host` is an unspecified address (`0.0.0.0`, `[::]`), the one bind
/// whose generated links never mention it.
fn is_wildcard(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_unspecified())
}

/// Whether a process is already listening on the loopback port. A connect
/// probe is used rather than a bind one, so a just-closed ssh forward — whose
/// accepted sockets sit in TIME_WAIT on this port for a while — does not make
/// a free port look occupied.
fn port_has_listener(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// Pick the local port for the tunnel.
///
/// The same number as the remote side when it is free here, so links the
/// server generates (`http://127.0.0.1:<port>/exchange/…`) resolve verbatim;
/// otherwise an ephemeral port, with [`Tunnel::rewrite`] translating them.
pub(super) fn choose_local_port(remote_port: u16) -> u16 {
    if !port_has_listener(remote_port) {
        return remote_port;
    }
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .unwrap_or(remote_port)
}

/// Bring the tunnel to a *working* server and return the token it accepts.
///
/// The token decides whether the probe means anything: an authenticated server
/// answers 401 to an anonymous request, which is indistinguishable from "not
/// running" at the socket. So the token is read first — and a read that fails
/// is kept, because otherwise it turns into exactly that 401 — and a missing
/// one is only recoverable by starting the server (it writes `auth.token` on
/// start).
pub(super) async fn bootstrap(
    remote: &Remote,
    tunnel: &Tunnel,
    explicit_token: Option<&str>,
    serve: bool,
    pipe: bool,
) -> Result<Option<String>> {
    let mut token_error = None;
    let mut token = match explicit_token {
        Some(token) => Some(token.to_string()),
        None => match read_token(remote).await {
            Ok(token) => Some(token),
            Err(err) => {
                token_error = Some(err);
                None
            }
        },
    };
    let mut source = token_source(explicit_token, token_error.as_ref());
    let mut last = probe(tunnel, token.as_deref()).await;
    if should_retry_rejected_auto_token(explicit_token, token_error.as_ref(), &last) {
        match read_token(remote).await {
            Ok(new_token) => {
                token = Some(new_token);
                source = token_source(None, None);
                last = probe(tunnel, token.as_deref()).await;
            }
            Err(err) => {
                token_error = Some(err);
                source = token_source(explicit_token, token_error.as_ref());
            }
        }
    }
    if last.is_ok() {
        return Ok(token);
    }
    if !serve {
        // The probe error is the diagnosis: it names a 401 when the server is
        // up but the token is stale, and a transport error when nothing is
        // listening behind the tunnel. A token that could not be read at all
        // explains a 401 on its own, so it leads the text.
        bail!("{}", no_answer(remote, last.as_ref().unwrap_err(), source));
    }

    remote.start_serve(pipe).await?;
    let deadline = tokio::time::Instant::now() + SERVE_STARTUP_TIMEOUT;
    loop {
        // The first start creates the token file, so it may appear a moment
        // after the server begins listening.
        if token.is_none() && explicit_token.is_none() {
            token = remote.token().await.ok();
        }
        // A probe blocks on its own timeout, so it is bounded by what is left
        // of the window rather than by the window plus one probe.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        last = match tokio::time::timeout(remaining, probe(tunnel, token.as_deref())).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "no answer within {}s",
                SERVE_STARTUP_TIMEOUT.as_secs()
            )),
        };
        if last.is_ok() {
            return Ok(token);
        }
        if tokio::time::Instant::now() >= deadline {
            // Part of the failure text, not a log line: it is printed before
            // the TUI starts, and the error is the one thing the user keeps.
            // Best effort and short of breath — the reason to be here may be
            // that ssh itself stopped answering.
            let tail = tokio::time::timeout(LOG_TAIL_TIMEOUT, remote.log_tail())
                .await
                .unwrap_or_default();
            let log = if tail.is_empty() {
                String::new()
            } else {
                format!("\n  --- remote jyc.log tail ---\n{tail}\n  --- end of log ---")
            };
            bail!(
                "jyc serve did not answer at {} on {} within {}s:\n  {}\n  \
                 start it by hand on {} to see why{log}",
                remote.bind,
                remote.host,
                SERVE_STARTUP_TIMEOUT.as_secs(),
                last.as_ref().unwrap_err(),
                remote.host
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The remote token, retried once.
///
/// A dropped ssh connection is indistinguishable downstream from "this hub has
/// no token": the probe then runs unauthenticated and the server answers 401,
/// which reads as "the token is wrong". One retry costs a few seconds and
/// removes the most common way that happens. Every failure is retried, not
/// just timeouts — [`Remote::capture`] flattens the cause into text, so
/// telling them apart would mean matching that text.
async fn read_token(remote: &Remote) -> Result<String> {
    match remote.token().await {
        Err(first) => match remote.token().await {
            Ok(token) => Ok(token),
            Err(second) => Err(anyhow::anyhow!("{first:#} (retry: {second:#})")),
        },
        ok => ok,
    }
}

/// Where the token the probe used came from.
///
/// A 401 says the server did not accept it, not why; naming the source is what
/// tells the user whether to look at `--token`, the remote file, or ssh.
enum TokenSource<'a> {
    /// Passed with `--token`, taken as given.
    Explicit,
    /// Read on the remote host with `token show`.
    Read,
    /// Reading it failed, so the probe ran without one.
    Unreadable(&'a anyhow::Error),
}

/// Which [`TokenSource`] the probe ran with.
fn token_source<'a>(
    explicit_token: Option<&str>,
    token_error: Option<&'a anyhow::Error>,
) -> TokenSource<'a> {
    match (explicit_token, token_error) {
        (Some(_), _) => TokenSource::Explicit,
        (None, Some(err)) => TokenSource::Unreadable(err),
        (None, None) => TokenSource::Read,
    }
}

/// Whether a freshly auto-read token was rejected and one re-read may fix it.
///
/// An explicit token is the user's assertion: retrying would hide a real
/// mismatch. An unreadable token was already retried by [`read_token`].
fn should_retry_rejected_auto_token(
    explicit_token: Option<&str>,
    token_error: Option<&anyhow::Error>,
    probe: &Result<()>,
) -> bool {
    explicit_token.is_none()
        && token_error.is_none()
        && probe.as_ref().err().is_some_and(is_unauthorized)
}

/// Whether the probe was answered with 401.
///
/// [`InspectClient`] flattens the status into its message
/// (`GET … returned 401 Unauthorized: …`), so this reads the text; the advice
/// [`no_answer`] gives about a rejected token is only worth printing when a
/// token was actually rejected.
pub(super) fn is_unauthorized(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains("returned 401"))
}

/// The message for a probe that failed and `--serve` was not given.
///
/// What the 401 advice may claim depends on whether a token was sent: with one,
/// a 401 means the hub refused it; without one, a 401 is what any hub answers,
/// and pointing at the token's freshness would be the misdiagnosis this text
/// exists to avoid.
fn no_answer(remote: &Remote, probe_err: &anyhow::Error, token: TokenSource<'_>) -> String {
    let mut text = String::new();
    if let TokenSource::Unreadable(err) = token {
        text.push_str(&format!(
            "could not read the dashboard token from {}: {err:#}\n",
            remote.host
        ));
    }
    text.push_str(&format!(
        "no jyc serve answering at {} on {}:\n  {probe_err:#}\n  token: {}",
        remote.bind,
        remote.host,
        match token {
            TokenSource::Explicit => "from --token".to_string(),
            TokenSource::Read =>
                format!("read with `{}`", remote.argv(&["token", "show"]).join(" ")),
            TokenSource::Unreadable(_) => "unreadable — the probe ran without one".to_string(),
        }
    ));
    text.push_str(&format!(
        "\n  start it there (`{} serve`), or pass -s/--serve to start one for you",
        remote.bin
    ));
    if remote.bind_fallback {
        text.push_str(
            "\n  note: the remote jyc does not support `config inspect-bind`, so the default \
             remote bind was assumed; pass --remote-bind if its [inspect].bind differs",
        );
    }
    match token {
        TokenSource::Unreadable(_) => text.push_str(
            "\n  if one is running, fix the read first (`--bin`, `--remote-data-dir`, or \
             `--token <token>`): without a token the hub answers 401 whatever its token is",
        ),
        _ if is_unauthorized(probe_err) => text.push_str(
            "\n  if it is running, check [inspect] enabled = true: that token was rejected — \
             the serve that answered made its token elsewhere (`--remote-data-dir`), or \
             rotated it after it started (restart it)",
        ),
        _ => text.push_str("\n  if it is running, check [inspect] enabled = true"),
    }
    text
}

/// Ask the server behind the tunnel for one authenticated snapshot.
async fn probe(tunnel: &Tunnel, token: Option<&str>) -> Result<()> {
    InspectClient::with_token(&tunnel.addr, token)
        .get_overview()
        .await
        .map(|_| ())
}

/// Resolve `-p` against the remote host.
///
/// The server takes the path verbatim (`POST /api/topics`), so it has to be
/// absolute by the time it leaves here; this machine's cwd and home mean
/// nothing on the other side.
pub(super) fn resolve_remote_path(path: Option<&str>, home: &str) -> Result<String> {
    let Some(path) = path else {
        return Ok(home.to_string());
    };
    let home = home.trim_end_matches('/');
    if let Some(rest) = path.strip_prefix('~') {
        // Only this user's own home: `~other/x` would need that user's home
        // dir, which this side has no way to learn.
        if !(rest.is_empty() || rest.starts_with('/')) {
            bail!("remote paths may not expand another user's home (got '{path}')");
        }
        let rest = rest.trim_start_matches('/');
        return Ok(if rest.is_empty() {
            home.to_string()
        } else {
            format!("{home}/{rest}")
        });
    }
    if !path.starts_with('/') {
        bail!("remote paths must be absolute or start with `~` (got '{path}')");
    }
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote() -> Remote {
        Remote {
            host: "devbox".into(),
            bin: "jyc".into(),
            data_dir: None,
            bind: "127.0.0.1:9876".into(),
            tunnel_bind: "127.0.0.1:9876".into(),
            bind_fallback: false,
            ssh_opts: Vec::new(),
        }
    }

    #[test]
    fn shell_quote_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
    }

    fn probe_error() -> anyhow::Error {
        anyhow::anyhow!("GET /api/state/overview returned 401 Unauthorized: auth_failed")
    }

    fn refused() -> anyhow::Error {
        anyhow::anyhow!("GET /api/state/overview request failed: connection refused")
    }

    #[test]
    fn inspect_bind_output_uses_the_first_nonempty_line() {
        assert_eq!(
            parse_inspect_bind_output("\n0.0.0.0:9816\n").unwrap(),
            "0.0.0.0:9816"
        );
        assert!(parse_inspect_bind_output("  \n").is_err());
    }

    #[test]
    fn old_remote_jyc_is_recognized_for_the_bind_fallback() {
        let old = anyhow::anyhow!(
            "`ssh devbox jyc config inspect-bind` failed: error: unrecognized subcommand 'inspect-bind'"
        );
        assert!(remote_lacks_inspect_bind(&old));
        let other =
            anyhow::anyhow!("`ssh devbox jyc config inspect-bind` failed: config parse error");
        assert!(!remote_lacks_inspect_bind(&other));
    }

    #[test]
    fn only_an_auto_read_token_is_re_read_after_a_401() {
        let rejected = || Err::<(), _>(probe_error());

        assert!(should_retry_rejected_auto_token(None, None, &rejected()));
        assert!(!should_retry_rejected_auto_token(
            Some("t"),
            None,
            &rejected()
        ));
        let read_err = anyhow::anyhow!("token read failed");
        assert!(!should_retry_rejected_auto_token(
            None,
            Some(&read_err),
            &rejected()
        ));
        assert!(!should_retry_rejected_auto_token(
            None,
            None,
            &Err::<(), _>(refused())
        ));
    }

    #[test]
    fn a_token_that_could_not_be_read_leads_the_failure_text() {
        let err = anyhow::anyhow!("`ssh devbox jyc token show` failed: jyc: command not found");
        let text = no_answer(&remote(), &probe_error(), TokenSource::Unreadable(&err));
        assert!(
            text.starts_with(
                "could not read the dashboard token from devbox: `ssh devbox jyc token show` failed"
            ),
            "{text}"
        );
        assert!(
            text.contains("unreadable — the probe ran without one"),
            "{text}"
        );
        // No token was sent, so there is nothing to call stale: the advice is
        // to fix the read, not to check whether the hub's token is current.
        assert!(text.contains("fix the read first"), "{text}");
        assert!(!text.contains("was rejected"), "{text}");
        assert!(!text.contains("rotated"), "{text}");
    }

    #[test]
    fn the_failure_text_names_the_token_it_tried() {
        let text = no_answer(&remote(), &probe_error(), TokenSource::Explicit);
        assert!(text.contains("token: from --token"), "{text}");
        assert!(!text.contains("could not read"), "{text}");
        assert!(text.contains("that token was rejected"), "{text}");

        let mut remote_with_data_dir = remote();
        remote_with_data_dir.data_dir = Some("/srv/jyc".into());
        let text = no_answer(&remote_with_data_dir, &probe_error(), TokenSource::Read);
        assert!(
            text.contains("token: read with `jyc --data-dir /srv/jyc token show`"),
            "{text}"
        );
    }

    #[test]
    fn a_transport_failure_is_not_dressed_up_as_a_token_problem() {
        let text = no_answer(&remote(), &refused(), TokenSource::Read);
        assert!(text.contains("connection refused"), "{text}");
        assert!(text.contains("[inspect] enabled = true"), "{text}");
        assert!(!text.contains("was rejected"), "{text}");
        assert!(!text.contains("rotated"), "{text}");
    }

    #[test]
    fn the_bind_fallback_tells_the_user_about_the_old_remote_command() {
        let mut old_remote = remote();
        old_remote.bind_fallback = true;
        let text = no_answer(&old_remote, &refused(), TokenSource::Read);
        assert!(
            text.contains("does not support `config inspect-bind`"),
            "{text}"
        );
        assert!(text.contains("--remote-bind"), "{text}");
    }

    #[test]
    fn the_token_source_says_where_the_probe_token_came_from() {
        let err = anyhow::anyhow!("boom");
        assert!(matches!(
            token_source(Some("t"), Some(&err)),
            TokenSource::Explicit
        ));
        assert!(
            matches!(token_source(None, Some(&err)), TokenSource::Unreadable(e) if std::ptr::eq(e, &err))
        );
        assert!(matches!(token_source(None, None), TokenSource::Read));
    }

    #[test]
    fn argv_carries_the_remote_data_dir_before_the_subcommand() {
        let mut remote = remote();
        remote.data_dir = Some("/srv/jyc data".into());
        assert_eq!(
            remote.argv(&["token", "show"]),
            vec!["jyc", "--data-dir", "/srv/jyc data", "token", "show"]
        );
    }

    #[test]
    fn argv_without_data_dir_uses_the_remote_default() {
        assert_eq!(remote().argv(&["serve"]), vec!["jyc", "serve"]);
    }

    #[test]
    fn bind_port_reads_the_port_off_the_remote_bind() {
        assert_eq!(remote().bind_port().unwrap(), 9876);
        assert_eq!(remote().bind_host().unwrap(), "127.0.0.1");
        let mut remote = remote();
        remote.bind = "0.0.0.0:9000".into();
        assert_eq!(remote.bind_port().unwrap(), 9000);
        remote.bind = "nonsense".into();
        assert!(remote.bind_port().is_err());
    }

    #[test]
    fn bind_parts_takes_a_bracketed_v6_literal_and_rejects_a_bare_one() {
        let mut remote = remote();
        remote.bind = "[::1]:9876".into();
        assert_eq!(remote.bind_host().unwrap(), "[::1]");
        assert_eq!(remote.bind_port().unwrap(), 9876);
        remote.bind = "::1:9876".into();
        let err = remote.bind_parts().unwrap_err();
        assert!(err.to_string().contains("bracket"), "{err}");
        remote.bind = ":9876".into();
        let err = remote.bind_parts().unwrap_err();
        assert!(err.to_string().contains("missing host"), "{err}");
    }

    #[test]
    fn remote_path_defaults_to_the_remote_home() {
        assert_eq!(resolve_remote_path(None, "/home/dev").unwrap(), "/home/dev");
    }

    #[test]
    fn remote_path_expands_tilde_against_the_remote_home() {
        assert_eq!(
            resolve_remote_path(Some("~/src/app"), "/home/dev").unwrap(),
            "/home/dev/src/app"
        );
        assert_eq!(
            resolve_remote_path(Some("~"), "/home/dev/").unwrap(),
            "/home/dev"
        );
    }

    #[test]
    fn remote_path_keeps_absolute_paths_and_rejects_relative_ones() {
        assert_eq!(
            resolve_remote_path(Some("/srv/app"), "/home/dev").unwrap(),
            "/srv/app"
        );
        let err = resolve_remote_path(Some("src/app"), "/home/dev").unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn remote_path_refuses_another_users_home() {
        let err = resolve_remote_path(Some("~other/src"), "/home/dev").unwrap_err();
        assert!(err.to_string().contains("another user"), "{err}");
    }

    #[test]
    fn rewrite_pair_appears_only_when_the_ports_differ() {
        assert_eq!(rewrite_pair(9876, 9876, "127.0.0.1"), None);
        assert_eq!(
            rewrite_pair(41234, 9876, "127.0.0.1"),
            Some((
                "http://127.0.0.1:9876".to_string(),
                "http://127.0.0.1:41234".to_string()
            ))
        );
    }

    #[test]
    fn rewrite_pair_follows_the_bind_host_but_skips_a_wildcard() {
        assert_eq!(
            rewrite_pair(41234, 9876, "localhost"),
            Some((
                "http://localhost:9876".to_string(),
                "http://127.0.0.1:41234".to_string()
            ))
        );
        assert_eq!(
            rewrite_pair(41234, 9876, "[::1]"),
            Some((
                "http://[::1]:9876".to_string(),
                "http://127.0.0.1:41234".to_string()
            ))
        );
        // A wildcard bind generates links with the remote's LAN IP, which this
        // machine cannot know: no rewrite to offer.
        assert_eq!(rewrite_pair(41234, 9876, "0.0.0.0"), None);
        assert_eq!(rewrite_pair(41234, 9876, "[::]"), None);
    }

    fn args_of(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn tunnel_args_compress_and_forward_to_the_remote_bind() {
        let remote = remote();
        let args = args_of(&tunnel_args(&remote, 41234));
        assert!(args.iter().any(|a| a == "-C"), "{args:?}");
        assert!(args.iter().any(|a| a == "-N"), "{args:?}");
        let forward = args.iter().position(|a| a == "-L").expect("no -L");
        assert_eq!(args[forward + 1], "127.0.0.1:41234:127.0.0.1:9876");
        // Target last, after every option.
        assert_eq!(args.last().unwrap(), "devbox");
    }

    #[test]
    fn tunnel_targets_loopback_for_a_wildcard_remote_bind() {
        let mut remote = remote();
        remote.bind = "0.0.0.0:9816".into();
        remote.tunnel_bind = normalize_inspect_client_addr(&remote.bind);
        assert_eq!(remote.bind_host().unwrap(), "0.0.0.0");
        let args = args_of(&tunnel_args(&remote, 41234));
        let forward = args.iter().position(|a| a == "-L").expect("no -L");
        assert_eq!(args[forward + 1], "127.0.0.1:41234:127.0.0.1:9816");
    }

    #[test]
    fn ssh_opt_wins_by_coming_after_the_defaults() {
        let mut remote = remote();
        remote.ssh_opts = vec!["Compression=no".into()];
        let args = args_of(&tunnel_args(&remote, 41234));
        let last_o = args
            .iter()
            .rposition(|a| a == "Compression=no")
            .expect("missing user option");
        let default_c = args.iter().position(|a| a == "-C").unwrap();
        assert!(last_o > default_c, "{args:?}");
        // And they reach one-shot ssh commands too.
        let one_shot: Vec<String> = remote
            .ssh("token show")
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            one_shot.contains(&"Compression=no".to_string()),
            "{one_shot:?}"
        );
    }

    #[test]
    fn tunnel_reopen_backoff_doubles_and_caps_at_thirty_seconds() {
        let mut backoff = TUNNEL_RETRY_MIN;
        let mut seen = Vec::new();
        for _ in 0..7 {
            seen.push(backoff);
            backoff = next_tunnel_backoff(backoff);
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 30, 30].map(Duration::from_secs));
    }

    #[tokio::test]
    async fn supervisor_reopens_a_dying_tunnel_until_aborted() {
        let opens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let opened = opens.clone();
        let open = move || {
            let opened = opened.clone();
            async move {
                opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok::<_, anyhow::Error>(Tunnel {
                    addr: "127.0.0.1:0".to_string(),
                    rewrite: None,
                    child: Command::new("sleep")
                        .arg("0.03")
                        .kill_on_drop(true)
                        .spawn()?,
                })
            }
        };
        let first = open().await.expect("first tunnel");
        let handle = tokio::spawn(async move { supervise_tunnel_loop(first, open).await });
        // Every generation lives ~30 ms; several must have come and gone by
        // the time this fires, even on a heavily loaded runner.
        tokio::time::sleep(Duration::from_millis(500)).await;
        handle.abort();
        let total = opens.load(std::sync::atomic::Ordering::SeqCst);
        assert!(total >= 3, "tunnel was reopened too few times: {total}");
    }

    #[test]
    fn backoff_resets_after_real_uptime_but_grows_after_instant_death() {
        let long_up = Duration::from_secs(10);
        let instant = Duration::from_millis(100);
        assert_eq!(
            next_backoff_after_death(Duration::from_secs(1), long_up),
            TUNNEL_RETRY_MIN
        );
        assert_eq!(
            next_backoff_after_death(Duration::from_secs(1), instant),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_backoff_after_death(TUNNEL_RETRY_MAX, instant),
            TUNNEL_RETRY_MAX
        );
    }

    #[test]
    fn tunnel_args_pin_the_requested_local_port() {
        // A reopen must rebind the very port the dashboard is aimed at.
        let args = tunnel_args(&remote(), 9876);
        assert!(
            args.iter()
                .any(|a| a.to_string_lossy() == "127.0.0.1:9876:127.0.0.1:9876"),
            "args: {args:?}"
        );
    }

    #[tokio::test]
    async fn failed_reopens_retry_then_recover() -> anyhow::Result<()> {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        let open = move || {
            let seen = seen.clone();
            async move {
                // The first two opens fail; the third succeeds and stays up.
                if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                    Err(anyhow::anyhow!("ssh is down"))
                } else {
                    Ok::<_, anyhow::Error>(Tunnel {
                        addr: "127.0.0.1:0".to_string(),
                        rewrite: None,
                        child: Command::new("sleep").arg("30").kill_on_drop(true).spawn()?,
                    })
                }
            }
        };
        let first = Tunnel {
            addr: "127.0.0.1:0".to_string(),
            rewrite: None,
            child: Command::new("sleep")
                .arg("0.03")
                .kill_on_drop(true)
                .spawn()?,
        };
        let handle = tokio::spawn(async move { supervise_tunnel_loop(first, open).await });
        // Two failures sleep 1s + 2s of backoff before the recovering open,
        // so three attempts must show up well within the deadline.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while attempts.load(std::sync::atomic::Ordering::SeqCst) < 3 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "supervisor never recovered"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Recovery parks the supervisor on the new tunnel: no further open
        // may run. Without this an opener that never succeeds passes the
        // loop above too, since `attempts >= 3` held from its third call on.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn open_refuses_to_spawn_when_the_local_port_is_occupied() -> anyhow::Result<()> {
        // No lock needed: the bail fires before ssh is ever spawned.
        let foreign = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = foreign.local_addr()?.port();
        let err = Tunnel::open(&remote(), port, port)
            .await
            .err()
            .expect("open must refuse a port another listener owns");
        assert!(
            err.to_string().contains("already in use"),
            "unexpected error: {err:#}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aborting_the_supervisor_kills_every_tunnel_child() -> anyhow::Result<()> {
        let pids = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = pids.clone();
        let open = move || {
            let recorded = recorded.clone();
            async move {
                // Long-lived: must still be running when the abort lands, or
                // the post-abort assertions pass even if abort killed nothing.
                let child = Command::new("sleep").arg("30").kill_on_drop(true).spawn()?;
                recorded
                    .lock()
                    .unwrap()
                    .push(child.id().expect("child just spawned"));
                Ok::<_, anyhow::Error>(Tunnel {
                    addr: "127.0.0.1:0".to_string(),
                    rewrite: None,
                    child,
                })
            }
        };
        // Short-lived: its death is what makes the supervisor reopen.
        let first_child = Command::new("sleep")
            .arg("0.03")
            .kill_on_drop(true)
            .spawn()?;
        let first_pid = first_child.id().expect("child just spawned");
        let first = Tunnel {
            addr: "127.0.0.1:0".to_string(),
            rewrite: None,
            child: first_child,
        };
        let handle = tokio::spawn(async move { supervise_tunnel_loop(first, open).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        let reopened = pids.lock().unwrap().clone();
        assert!(!reopened.is_empty(), "supervisor never reopened the tunnel");
        for pid in &reopened {
            assert!(pid_alive(*pid), "tunnel child {pid} died before the abort");
        }
        handle.abort();
        let _ = handle.await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        for pid in std::iter::once(first_pid).chain(reopened) {
            while pid_alive(pid) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "tunnel child {pid} still alive after abort"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
