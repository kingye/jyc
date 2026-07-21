use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

/// Arguments for the `jyc stop` command.
#[derive(Debug, clap::Args)]
pub struct StopArgs {
    /// Force stop (SIGKILL instead of SIGTERM)
    #[arg(long)]
    pub force: bool,
}

/// Run the `jyc stop` command: read the PID file and send a signal.
pub async fn run(args: &StopArgs, workdir: &Path) -> Result<()> {
    let pid_path = workdir.join("jyc.pid");

    // Read PID file
    let pid_str = tokio::fs::read_to_string(&pid_path)
        .await
        .with_context(|| {
            format!(
                "Failed to read PID file at {}. Is jyc serve running?",
                pid_path.display()
            )
        })?;

    let pid: u32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("Invalid PID in file: {}", pid_path.display()))?;

    // Check if process is running
    if !pid_exists(pid) {
        tracing::warn!(pid, path = %pid_path.display(), "Process not running, cleaning up stale PID file");
        tokio::fs::remove_file(&pid_path).await.ok();
        anyhow::bail!("jyc serve is not running (stale PID {pid})");
    }

    // Determine signal
    let signal = if args.force {
        libc::SIGKILL
    } else {
        libc::SIGTERM
    };

    // Send the signal
    let signal_name = if args.force { "SIGKILL" } else { "SIGTERM" };
    tracing::info!(pid, signal = %signal_name, "Sending signal to jyc serve");

    let ret = unsafe { libc::kill(pid as i32, signal) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("Failed to send {signal_name} to PID {pid}: {err}");
    }

    // Wait briefly for process to exit (poll every 200ms, up to 3s)
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !pid_exists(pid) {
            exited = true;
            break;
        }
    }

    // Remove PID file
    tokio::fs::remove_file(&pid_path).await.ok();

    if exited {
        tracing::info!(pid, "jyc serve stopped");
        println!("jyc serve stopped (PID {pid})");
        Ok(())
    } else if !args.force {
        // Process didn't exit in time, suggest --force
        tracing::warn!(pid, "Process did not exit after SIGTERM within 3s");
        println!(
            "jyc serve (PID {pid}) did not stop within 3 seconds after SIGTERM.\n\
             Use `jyc stop --force` to force kill."
        );
        Ok(())
    } else {
        // SIGKILL was sent but process still didn't exit (shouldn't happen)
        tracing::warn!(pid, "Process still alive after SIGKILL");
        println!("Warning: PID {pid} still appears to be running after SIGKILL.");
        Ok(())
    }
}

/// Check whether a process with the given PID is running.
#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    ret == 0
}

#[cfg(not(unix))]
fn pid_exists(_pid: u32) -> bool {
    // Non-Unix fallback: we can't easily check, assume it exists
    true
}
