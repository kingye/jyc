//! `jyc connect <host>` — the hub runs on another machine, the dashboard runs
//! here.
//!
//! The transport is an `ssh -N -L` child process: the dashboard keeps speaking
//! plain HTTP/WS to `127.0.0.1:<local port>`, and ssh forwards that to the
//! remote's own inspect bind. Nothing is exposed on the remote beyond the
//! loopback bind it already had, the tunnel is loopback on both ends (so no
//! TLS is needed), and the SSH config, keys, agent and jump hosts the user
//! already has apply unchanged.

use anyhow::{Context, Result, bail};
use clap::Args;
use jyc_inspect::client::InspectClient;
use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

use super::OpenArgs;

/// How long the local end of the tunnel may take to accept connections.
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a remote `jyc serve` we started may take to answer.
const SERVE_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

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

    /// Inspect server address on the remote host
    #[arg(long, default_value = "127.0.0.1:9876")]
    pub remote_bind: String,

    /// Authorization token (default: read from the remote host)
    #[arg(long, env = "JYC_DASHBOARD_TOKEN")]
    pub token: Option<String>,

    /// Open a directory on the remote host as a topic and go straight to chat
    #[command(flatten)]
    pub open: OpenArgs,
}

/// The remote end of a `jyc connect` session.
pub(super) struct Remote {
    host: String,
    bin: String,
    data_dir: Option<String>,
    /// `host:port` the remote inspect server listens on.
    bind: String,
}

impl Remote {
    pub(super) fn from_args(args: &ConnectArgs) -> Self {
        Self {
            host: args.host.clone(),
            bin: args.bin.clone(),
            data_dir: args.remote_data_dir.clone(),
            bind: args.remote_bind.clone(),
        }
    }

    /// The port the remote inspect server listens on — the port the tunnel
    /// forwards to.
    pub(super) fn bind_port(&self) -> Result<u16> {
        self.bind
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
            .with_context(|| format!("invalid --remote-bind '{}': expected host:port", self.bind))
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
            .arg(&self.host)
            .arg(remote_cmd);
        cmd
    }

    /// Run a command over ssh and return its trimmed stdout.
    async fn capture(&self, remote_cmd: &str) -> Result<String> {
        let out = self
            .ssh(remote_cmd)
            .stdin(Stdio::null())
            .output()
            .await
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
        self.capture(&command_line(&self.argv(&["token", "show"])))
            .await
            .context("could not read the dashboard token on the remote host")
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
    /// `setsid nohup` detaches it from this ssh session: it has no controlling
    /// terminal and does not take SIGHUP when the session closes, so it keeps
    /// running — the dashboard owns a tunnel, not the hub.
    pub(super) async fn start_serve(&self, pipe: bool) -> Result<()> {
        let mut argv = self.argv(&["serve"]);
        if pipe {
            argv.push("--pipe".to_string());
        }
        let cmd = format!("setsid nohup {} >/dev/null 2>&1 &", command_line(&argv));
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

/// A live `ssh -N -L` forward.
///
/// Dropping it ends the tunnel; the remote hub keeps running — it may have
/// other dashboards, channels and scheduled jobs using it.
pub(super) struct Tunnel {
    /// `127.0.0.1:<port>` — what the dashboard's HTTP and WS clients talk to.
    pub(super) addr: String,
    /// `(from, to)` when the local port differs from the remote one: publish
    /// links carry the remote's own `http://127.0.0.1:<port>`, which does not
    /// resolve here, so message text is translated on the way in.
    pub(super) rewrite: Option<(String, String)>,
    /// Held for its `kill_on_drop`.
    _child: Child,
}

impl Tunnel {
    /// Open the forward and wait until ssh accepts connections on it.
    pub(super) async fn open(remote: &Remote, local_port: u16, remote_port: u16) -> Result<Self> {
        let addr = format!("127.0.0.1:{local_port}");
        let args: Vec<OsString> = vec![
            "-N".into(),
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
            "-L".into(),
            format!("127.0.0.1:{local_port}:{}", remote.bind).into(),
            remote.host.clone().into(),
        ];

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
                break;
            }
            if let Some(status) = child.try_wait()? {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr).await;
                }
                bail!(
                    "ssh tunnel to {} exited ({status}): {}\n  \
                     the tunnel needs non-interactive auth — check `ssh {} true` and your agent",
                    remote.host,
                    stderr.trim(),
                    remote.host
                );
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

        let rewrite = rewrite_pair(local_port, remote_port);
        Ok(Self {
            addr,
            rewrite,
            _child: child,
        })
    }
}

/// The message-text translation a tunnel needs, if any: publish links carry
/// the remote's own loopback URL, which only resolves here when the ports
/// happen to match.
fn rewrite_pair(local_port: u16, remote_port: u16) -> Option<(String, String)> {
    (local_port != remote_port).then(|| {
        (
            format!("http://127.0.0.1:{remote_port}"),
            format!("http://127.0.0.1:{local_port}"),
        )
    })
}

/// Pick the local port for the tunnel.
///
/// The same number as the remote side when it is free here, so links the
/// server generates (`http://127.0.0.1:<port>/exchange/…`) resolve verbatim;
/// otherwise an ephemeral port, with [`Tunnel::rewrite`] translating them.
pub(super) fn choose_local_port(remote_port: u16) -> u16 {
    let free = |port: u16| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();
    if free(remote_port) {
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
/// running" at the socket. So the token is read first, and a missing one is
/// only recoverable by starting the server (it writes `auth.token` on start).
pub(super) async fn bootstrap(
    remote: &Remote,
    tunnel: &Tunnel,
    explicit_token: Option<&str>,
    serve: bool,
    pipe: bool,
) -> Result<Option<String>> {
    let mut token = match explicit_token {
        Some(token) => Some(token.to_string()),
        None => remote.token().await.ok(),
    };
    let mut last = probe(tunnel, token.as_deref()).await;
    if last.is_ok() {
        return Ok(token);
    }
    if !serve {
        // The probe error is the diagnosis: it names a 401 when the server is
        // up but the token is stale, and a transport error when nothing is
        // listening behind the tunnel.
        bail!(
            "no jyc serve answering at {} on {}:\n  {}\n  \
             start it there (`{} serve`), or pass -s/--serve to start one for you\n  \
             if it is running, check [inspect] enabled = true — and that the token is current",
            remote.bind,
            remote.host,
            last.as_ref().unwrap_err(),
            remote.bin
        );
    }

    remote.start_serve(pipe).await?;
    let deadline = tokio::time::Instant::now() + SERVE_STARTUP_TIMEOUT;
    loop {
        // The first start creates the token file, so it may appear a moment
        // after the server begins listening.
        if token.is_none() && explicit_token.is_none() {
            token = remote.token().await.ok();
        }
        last = probe(tunnel, token.as_deref()).await;
        if last.is_ok() {
            return Ok(token);
        }
        if tokio::time::Instant::now() >= deadline {
            let tail = remote.log_tail().await;
            if !tail.is_empty() {
                eprintln!("--- remote jyc.log ---\n{tail}\n--- end of log ---");
            }
            bail!(
                "jyc serve did not answer at {} on {} within {}s:\n  {}\n  \
                 see its log on {} (path shown when --remote-data-dir is given)",
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
        }
    }

    #[test]
    fn shell_quote_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
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
        let mut remote = remote();
        remote.bind = "0.0.0.0:9000".into();
        assert_eq!(remote.bind_port().unwrap(), 9000);
        remote.bind = "nonsense".into();
        assert!(remote.bind_port().is_err());
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
    fn rewrite_pair_appears_only_when_the_ports_differ() {
        assert_eq!(rewrite_pair(9876, 9876), None);
        assert_eq!(
            rewrite_pair(41234, 9876),
            Some((
                "http://127.0.0.1:9876".to_string(),
                "http://127.0.0.1:41234".to_string()
            ))
        );
    }
}
