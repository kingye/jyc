//! GitHub status watcher for `/gh on` dashboard integration.
//!
//! Spawns `gh pr status` and `gh run list --status in_progress`
//! inside a topic directory and returns a snapshot that the TUI can render.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;

const GH_TIMEOUT: Duration = Duration::from_secs(5);

static ANSI_RE: OnceLock<Regex> = OnceLock::new();

fn ansi_re() -> &'static Regex {
    ANSI_RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").expect("ANSI regex is valid"))
}

/// Snapshot of GitHub status for one topic.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GhSnapshot {
    /// UTC timestamp of the fetch attempt.
    pub fetched_at: DateTime<Utc>,
    /// Raw output lines from `gh pr status`.
    pub prs: Vec<String>,
    /// Raw output lines from `gh run list --status in_progress`.
    pub runs: Vec<String>,
    /// Error message if either gh command failed or is unavailable.
    pub error: Option<String>,
}

impl GhSnapshot {
    /// Create an empty snapshot with the current timestamp.
    pub fn empty() -> Self {
        Self {
            fetched_at: Utc::now(),
            ..Default::default()
        }
    }
}

/// Fetch a fresh snapshot from `gh` running in `cwd`.
///
/// `gh_bin` is usually `PathBuf::from("gh")`; tests pass a mock executable.
pub async fn fetch_snapshot(cwd: &Path, gh_bin: &Path) -> GhSnapshot {
    let mut snapshot = GhSnapshot::empty();

    match run_gh(cwd, gh_bin, &["pr", "status", "--no-color"]).await {
        Ok(lines) => snapshot.prs = lines,
        Err(e) => snapshot.error = Some(e.to_string()),
    }

    match run_gh(
        cwd,
        gh_bin,
        &["run", "list", "--status", "in_progress", "--no-color"],
    )
    .await
    {
        Ok(lines) => snapshot.runs = lines,
        Err(e) => snapshot.error = Some(e.to_string()),
    }

    snapshot
}

/// Render a snapshot into human-readable lines for the TUI.
pub fn render_snapshot_lines(snap: &GhSnapshot) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "Last refresh: {}",
        snap.fetched_at.format("%Y-%m-%d %H:%M:%S UTC")
    ));

    if let Some(ref err) = snap.error {
        lines.push(format!("Error: {}", err));
    }

    lines.push(String::from("PRs"));
    if snap.prs.is_empty() {
        lines.push(String::from("  (none)"));
    } else {
        lines.extend(snap.prs.clone());
    }

    lines.push(String::new());
    lines.push(String::from("Runs"));
    if snap.runs.is_empty() {
        lines.push(String::from("  (none)"));
    } else {
        lines.extend(snap.runs.clone());
    }

    lines
}

async fn run_gh(cwd: &Path, gh_bin: &Path, args: &[&str]) -> Result<Vec<String>> {
    let mut cmd = Command::new(gh_bin);
    cmd.current_dir(cwd)
        .args(args)
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("CLICOLOR_FORCE", "0")
        .env("COLUMNS", "200")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn gh at {:?}", gh_bin))?;

    let result = tokio::time::timeout(GH_TIMEOUT, child.wait_with_output())
        .await
        .context("gh command timed out")?
        .context("gh command failed to run")?;

    let stderr = String::from_utf8_lossy(&result.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&result.stdout);

    if !result.status.success() {
        let msg = if stderr.is_empty() {
            stdout.trim().to_string()
        } else {
            stderr
        };
        anyhow::bail!("gh failed: {}", msg);
    }

    Ok(stdout
        .lines()
        .map(|line| ansi_re().replace_all(line, "").to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    #[cfg(unix)]
    #[tokio::test]
    async fn test_fetch_snapshot_success() {
        let tmp = TempDir::new().unwrap();
        let gh = tmp.path().join("gh");
        fs::write(
            &gh,
            r#"#!/bin/sh
if [ "$1" = "pr" ] && [ "$2" = "status" ]; then
  echo "Current branch"
  echo "  #42  feat: gh status"
elif [ "$1" = "run" ] && [ "$2" = "list" ]; then
  echo "ID  NAME  STATUS  BRANCH"
  echo "123  ci  in_progress  main"
else
  echo "unknown" >&2
  exit 1
fi
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&gh).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&gh, perms).unwrap();

        let cwd = tmp.path();
        let snap = fetch_snapshot(cwd, &gh).await;

        assert!(snap.error.is_none(), "unexpected error: {:?}", snap.error);
        assert!(snap.prs.iter().any(|l| l.contains("#42")));
        assert!(snap.runs.iter().any(|l| l.contains("in_progress")));
    }

    #[tokio::test]
    async fn test_fetch_snapshot_missing_gh() {
        let tmp = TempDir::new().unwrap();
        let snap = fetch_snapshot(tmp.path(), Path::new("/does/not/exist/gh")).await;
        assert!(snap.error.is_some());
        assert!(snap.prs.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_fetch_snapshot_gh_error() {
        let tmp = TempDir::new().unwrap();
        let gh = tmp.path().join("gh");
        fs::write(
            &gh,
            r#"#!/bin/sh
echo "not a git repository" >&2
exit 1
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&gh).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&gh, perms).unwrap();

        let snap = fetch_snapshot(tmp.path(), &gh).await;
        assert!(snap.error.is_some());
    }

    #[test]
    fn test_render_snapshot_lines() {
        let snap = GhSnapshot {
            fetched_at: Utc::now(),
            prs: vec!["#42  feat: gh status".into()],
            runs: vec![],
            error: Some("boom".into()),
        };
        let lines = render_snapshot_lines(&snap);
        assert!(lines.iter().any(|l| l.contains("PRs")));
        assert!(lines.iter().any(|l| l.contains("Runs")));
        assert!(lines.iter().any(|l| l.contains("boom")));
    }
}
