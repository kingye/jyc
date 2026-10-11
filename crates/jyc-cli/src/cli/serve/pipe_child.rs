//! Child-process supervision for `jyc-pipe` (`jyc serve --pipe`).
//!
//! The peripheral message pipe ([`jyc-pipe`](../../../../../jyc-channels/src/bin/jyc-pipe.rs))
//! hosts the pipe-only channel adapters (feishu, github, gitee, wecom,
//! wecom_bot, wecomkf, email); `jyc serve` skips exactly those channels
//! in-process. Without a pipe running they receive nothing, so operators
//! pair the two processes by hand (systemd, the compose file) — this module
//! lets `jyc serve --pipe` do it in one command.
//!
//! The child is *not* restarted when it exits: a channel crashing must never
//! take the agent core down (that is the point of the process split), and
//! the pipe reconnects to the hub on its own. A dead pipe is logged at
//! `error` level so it does not stay silent. See
//! `docs/architecture/pipe-split.md`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

/// How long the child gets to exit after SIGTERM before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The `jyc-pipe` binary to run: the one next to the running `jyc` (cargo
/// builds and the Docker image both put them side by side), else whatever
/// `PATH` resolves.
pub(crate) fn pipe_binary() -> PathBuf {
    let name = format!("jyc-pipe{}", std::env::consts::EXE_SUFFIX);
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        let sibling = dir.join(&name);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(name)
}

/// Command line for the pipe.
///
/// `--workdir` and `--config` are both passed on purpose: the pipe resolves
/// its config through the same helper the hub uses, where `--workdir` alone
/// means "read `<workdir>/config.toml`" — not necessarily the file the hub
/// loaded. The log destination is left alone, so the pipe keeps writing its
/// own `<data_home>/jyc-pipe.log`.
pub(crate) fn pipe_command(bin: &Path, workdir: &Path, config_path: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("--workdir")
        .arg(workdir)
        .arg("--config")
        .arg(config_path);
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    cmd
}

/// A running `jyc-pipe` child, stopped together with the hub.
pub(crate) struct PipeChild {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    supervisor: tokio::task::JoinHandle<()>,
}

impl PipeChild {
    /// Spawn `jyc-pipe` against the hub's own workdir and config file.
    pub(crate) fn spawn(workdir: &Path, config_path: &Path) -> Result<Self> {
        let bin = pipe_binary();
        let child = pipe_command(&bin, workdir, config_path)
            .spawn()
            .with_context(|| format!("Failed to spawn {}", bin.display()))?;
        let pid = child.id().unwrap_or(0);
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let supervisor = tokio::spawn(supervise(child, stop_rx));
        tracing::info!(pid, bin = %bin.display(), "jyc-pipe child started");
        Ok(Self {
            stop: Some(stop),
            supervisor,
        })
    }

    /// Ask the child to stop and wait for it (SIGTERM, then SIGKILL).
    pub(crate) async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            // A closed channel (supervisor gone) is fine — dropping the
            // sender also triggers the stop path.
            let _ = stop.send(());
        }
        let _ = self.supervisor.await;
    }
}

/// Owns the child: report its exit, or stop it when asked.
async fn supervise(mut child: Child, stop: tokio::sync::oneshot::Receiver<()>) {
    tokio::select! {
        status = child.wait() => {
            // No restart — see the module docs.
            tracing::error!(?status, "jyc-pipe exited; pipe-only channels are offline");
        }
        _ = stop => {
            let pid = child.id().unwrap_or(0);
            terminate(pid);
            if tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
                tracing::warn!(pid, "jyc-pipe did not exit after SIGTERM; killing it");
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    }
}

/// SIGTERM the child — the same signal `jyc stop` uses on the hub. Windows
/// has no equivalent here, so the grace timeout below kills the child.
#[cfg(unix)]
fn terminate(pid: u32) {
    // SAFETY: `kill` takes plain integers and is async-signal-safe.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn terminate(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_passes_the_hub_workdir_and_config() {
        let cmd = pipe_command(
            Path::new("/usr/local/bin/jyc-pipe"),
            Path::new("/data/jyc"),
            Path::new("/data/jyc/config.toml"),
        );
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--workdir",
                "/data/jyc",
                "--config",
                "/data/jyc/config.toml"
            ]
        );
    }

    /// The stub child: `sh -c <script>`, same wiring as the real spawn.
    #[cfg(unix)]
    fn stub_child(script: &str) -> Child {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
        cmd.spawn().expect("spawn stub child")
    }

    /// A child that exits on its own is observed (and not waited on
    /// forever): the supervisor finishes without a stop request.
    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_reports_a_child_that_exits() {
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let supervisor = tokio::spawn(supervise(stub_child("exit 7"), stop_rx));
        tokio::time::timeout(Duration::from_secs(10), supervisor)
            .await
            .expect("supervisor finishes on its own")
            .expect("supervisor joins");
        drop(stop);
    }

    /// The stop path terminates a long-running child well inside the grace
    /// period (stub `sh` does not trap SIGTERM).
    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_stops_a_running_child() {
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let supervisor = tokio::spawn(supervise(stub_child("sleep 30"), stop_rx));
        stop.send(()).expect("supervisor waits for the stop signal");
        tokio::time::timeout(Duration::from_secs(10), supervisor)
            .await
            .expect("supervisor stops the child")
            .expect("supervisor joins");
    }
}
