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

    // Wait for process to exit (poll every 200ms, up to 10s)
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
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
        tracing::warn!(pid, "Process did not exit after SIGTERM within 10s");
        println!(
            "jyc serve (PID {pid}) did not stop within 10 seconds after SIGTERM.\n\
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_stop_missing_pid_file() {
        let tmp = TempDir::new().unwrap();
        let args = StopArgs { force: false };
        let err = run(&args, tmp.path()).await.unwrap_err().to_string();
        assert!(
            err.contains("Failed to read PID file"),
            "expected PID file error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_stop_invalid_pid_format() {
        let tmp = TempDir::new().unwrap();
        let pid_path = tmp.path().join("jyc.pid");
        tokio::fs::write(&pid_path, "not_a_number").await.unwrap();
        let args = StopArgs { force: false };
        let err = run(&args, tmp.path()).await.unwrap_err().to_string();
        assert!(
            err.contains("Invalid PID"),
            "expected Invalid PID error, got: {err}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_stop_stale_pid_file_cleaned_up() {
        let tmp = TempDir::new().unwrap();
        let pid_path = tmp.path().join("jyc.pid");
        // Use a very large PID that won't exist on any typical system
        tokio::fs::write(&pid_path, "999999999").await.unwrap();
        assert!(pid_path.exists());

        let args = StopArgs { force: false };
        let err = run(&args, tmp.path()).await.unwrap_err().to_string();
        assert!(
            err.contains("stale PID"),
            "expected stale PID error, got: {err}"
        );
        // Stale PID file should be cleaned up
        assert!(!pid_path.exists(), "stale PID file should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn test_pid_exists_returns_false_for_non_existent_pid() {
        // PID 999999999 should not exist on any typical system
        assert!(!pid_exists(999999999));
    }
}
