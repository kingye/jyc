use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

/// Arguments for the `jyc stop` command.
#[derive(Debug, clap::Args)]
pub struct StopArgs {
    /// Force stop (SIGKILL instead of SIGTERM)
    #[arg(long)]
    pub force: bool,
    /// Grace period (seconds) to wait for the process to exit after
    /// the signal. Defaults to 10.
    #[arg(long)]
    pub wait: Option<u64>,
}

/// Run the `jyc stop` command: read the PID file and send a signal.
pub async fn run(args: &StopArgs, data_dir: &Path) -> Result<()> {
    let pid_path = data_dir.join("jyc.pid");

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

    // Wait for process to exit (poll every 200ms, up to --wait seconds)
    let wait_secs = args.wait.unwrap_or(10);
    let deadline = std::time::Instant::now() + Duration::from_secs(wait_secs);
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
        tracing::warn!(
            pid,
            wait_secs,
            "Process did not exit after SIGTERM within the grace period"
        );
        println!(
            "jyc serve (PID {pid}) did not stop within {wait_secs} seconds after SIGTERM.\n\
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
///
/// A zombie is already dead even though `kill(pid, 0)` still succeeds until
/// its parent reaps it, so platform process state must be checked too.
#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    pid_signal_exists(pid) && !pid_is_zombie(pid)
}

#[cfg(unix)]
fn pid_signal_exists(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but the caller may not signal it.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "linux")]
fn pid_is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else {
        // If the status cannot be read, keep the `kill(pid, 0)` result rather
        // than treating an inspectable process as gone.
        return false;
    };
    matches!(proc_stat_state(&stat), Some(b'Z' | b'X' | b'x'))
}

/// Parse the one-letter process state from `/proc/<pid>/stat`.
///
/// The command name is parenthesized and may contain spaces or parentheses,
/// so find the final `)` before reading the state field.
#[cfg(target_os = "linux")]
fn proc_stat_state(stat: &[u8]) -> Option<u8> {
    let comm_end = stat.iter().rposition(|&byte| byte == b')')?;
    stat.get(comm_end + 2).copied()
}

#[cfg(target_os = "macos")]
fn pid_is_zombie(pid: u32) -> bool {
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let ret = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if ret != size {
        // Without a readable status, keep the `kill(pid, 0)` result rather
        // than treating an inspectable process as gone.
        return false;
    }
    info.pbsi_status == libc::SZOMB
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn pid_is_zombie(_pid: u32) -> bool {
    false
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
        let args = StopArgs {
            force: false,
            wait: None,
        };
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
        let args = StopArgs {
            force: false,
            wait: None,
        };
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

        let args = StopArgs {
            force: false,
            wait: None,
        };
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

    #[cfg(target_os = "linux")]
    #[test]
    fn test_proc_stat_state_uses_last_parenthesis_in_comm() {
        let stat: &[u8] = b"52662 (jyc serve) Z 1 52661 52662 4096 0";
        assert_eq!(proc_stat_state(stat), Some(b'Z'));
        let stat: &[u8] = b"52662 (jyc (worker)) S 1 52661 52662 4096 0";
        assert_eq!(proc_stat_state(stat), Some(b'S'));
        let stat: &[u8] = b"52662 (jyc \xff) Z 1 52661 52662 4096 0";
        assert_eq!(proc_stat_state(stat), Some(b'Z'));
        assert_eq!(proc_stat_state(b"52662 jyc"), None);
    }

    #[cfg(target_os = "linux")]
    fn wait_for_child_exit_without_reaping(pid: libc::pid_t) {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            let ret = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if ret == 0 {
                return;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                panic!("waitid failed for child {pid}: {err}");
            }
        }
    }

    #[cfg(target_os = "linux")]
    struct ChildReaper(libc::pid_t);

    #[cfg(target_os = "linux")]
    impl Drop for ChildReaper {
        fn drop(&mut self) {
            loop {
                let ret = unsafe { libc::waitpid(self.0, std::ptr::null_mut(), 0) };
                if ret != -1 {
                    return;
                }
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    return;
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_pid_exists_returns_false_for_zombie() {
        unsafe {
            let pid = libc::fork();
            if pid == 0 {
                libc::_exit(0);
            }
            assert!(pid > 0, "fork failed");
            let _child = ChildReaper(pid);

            // WNOWAIT waits for the exit without reaping, leaving the child
            // in the zombie state this function must recognize.
            wait_for_child_exit_without_reaping(pid);
            assert!(
                !pid_exists(pid as u32),
                "a zombie PID must count as stopped"
            );
        }
    }

    #[test]
    fn stop_args_wait_flag_parsing() {
        use clap::{Args, Command, FromArgMatches};
        let m = StopArgs::augment_args(Command::new("stop"))
            .try_get_matches_from(["stop"])
            .unwrap();
        let a = StopArgs::from_arg_matches(&m).unwrap();
        assert_eq!(a.wait, None);
        let m = StopArgs::augment_args(Command::new("stop"))
            .try_get_matches_from(["stop", "--wait", "30"])
            .unwrap();
        let a = StopArgs::from_arg_matches(&m).unwrap();
        assert_eq!(a.wait, Some(30));
    }
}
