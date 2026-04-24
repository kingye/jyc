use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use anyhow::Result;

use crate::channels::types::ChannelPattern;

/// Start the idle cleanup sweep task for a channel.
///
/// Each channel runs its own independent sweep task that periodically scans
/// the workspace for idle threads and removes specified subdirectories.
pub async fn start_idle_cleanup_sweep(
    workspace_dir: PathBuf,
    patterns: Vec<ChannelPattern>,
    cancel: CancellationToken,
) {
    let enabled_patterns: Vec<(String, &ChannelPattern)> = patterns
        .iter()
        .filter(|p| p.idle_cleanup.as_ref().map_or(false, |c| c.enabled))
        .map(|p| (p.name.clone(), p))
        .collect();

    if enabled_patterns.is_empty() {
        return;
    }

    let interval_secs = enabled_patterns
        .iter()
        .filter_map(|(_, p)| p.idle_cleanup.as_ref())
        .map(|c| c.interval_secs)
        .min()
        .unwrap_or(300);

    let pattern_map: std::collections::HashMap<String, &ChannelPattern> = enabled_patterns
        .into_iter()
        .collect();

    info!(
        workspace_dir = %workspace_dir.display(),
        pattern_count = pattern_map.len(),
        scan_interval_secs = interval_secs,
        "Starting idle cleanup sweep"
    );

    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Idle cleanup sweep cancelled");
                break;
            }
            _ = interval.tick() => {
                if let Err(e) = sweep_once(&workspace_dir, &pattern_map).await {
                    warn!(error = %e, "Idle cleanup sweep failed");
                }
            }
        }
    }
}

async fn sweep_once(
    workspace_dir: &Path,
    pattern_map: &std::collections::HashMap<String, &ChannelPattern>,
) -> Result<()> {
    let mut entries = fs::read_dir(workspace_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let jyc_dir = path.join(".jyc");
        if !jyc_dir.exists() {
            continue;
        }

        let thread_name = entry.file_name().to_string_lossy().to_string();

        if jyc_dir.join("question-sent.flag").exists() {
            continue;
        }

        if jyc_dir.join("idle-cleanup-skip.flag").exists() {
            tracing::debug!(thread = %thread_name, "Skipping thread with idle-cleanup-skip.flag");
            continue;
        }

        let pattern_file = jyc_dir.join("pattern");
        let pattern_name = if pattern_file.exists() {
            fs::read_to_string(&pattern_file).await?.trim().to_string()
        } else {
            continue;
        };

        let idle_config = match pattern_map.get(&pattern_name) {
            Some(p) => p.idle_cleanup.as_ref(),
            None => continue,
        };

        let idle_config = match idle_config {
            Some(c) => c,
            None => continue,
        };

        let last_active = match fs::metadata(&jyc_dir).await {
            Ok(meta) => match meta.modified() {
                Ok(mtime) => {
                    let dt: chrono::DateTime<chrono::Utc> = mtime.into();
                    dt
                }
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        let now = chrono::Utc::now();
        let idle_duration = now.signed_duration_since(last_active);
        let timeout = chrono::Duration::seconds(idle_config.timeout_secs as i64);

        let idle_cleaned_flag = jyc_dir.join("idle-cleaned.flag");

        if idle_duration > timeout {
            if idle_cleaned_flag.exists() {
                continue;
            }

            let mut cleaned_any = false;
            for clean_path in &idle_config.clean_paths {
                if !is_safe_path(clean_path) {
                    warn!(
                        thread = %thread_name,
                        path = %clean_path,
                        "Skipping unsafe clean_path (contains path traversal)"
                    );
                    continue;
                }

                let target = path.join(clean_path);
                if target.exists() {
                    cleaned_any = true;
                    match fs::remove_dir_all(&target).await {
                        Ok(_) => {
                            info!(
                                thread = %thread_name,
                                path = %target.display(),
                                idle_secs = idle_duration.num_seconds(),
                                "Cleaned idle thread subdirectory"
                            );
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                thread = %thread_name,
                                path = %target.display(),
                                "Failed to clean idle thread subdirectory"
                            );
                        }
                    }
                }
            }

            if cleaned_any {
                if let Err(e) = fs::write(&idle_cleaned_flag, "").await {
                    warn!(error = %e, "Failed to write idle-cleaned.flag");
                }
            }
        } else if idle_cleaned_flag.exists() {
            if let Err(e) = fs::remove_file(&idle_cleaned_flag).await {
                warn!(error = %e, "Failed to remove idle-cleaned.flag");
            } else {
                info!(thread = %thread_name, "Thread became active, removed idle-cleaned.flag");
            }
        }
    }

    Ok(())
}

fn is_safe_path(path: &str) -> bool {
    !path.contains("..") && !path.contains('/') && !path.contains('\\')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_idle_cleanup_skips_question_sent_flag() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        let thread_dir = workspace.join("waiting-thread");
        fs::create_dir_all(thread_dir.join(".jyc")).await.unwrap();
        fs::create_dir_all(thread_dir.join("repo")).await.unwrap();

        fs::write(thread_dir.join(".jyc").join("pattern"), "developer").await.unwrap();
        fs::write(thread_dir.join(".jyc").join("question-sent.flag"), "").await.unwrap();

        let old_time = SystemTime::now() - std::time::Duration::from_secs(86400 * 2);
        let old_filetime = filetime::FileTime::from_system_time(old_time);
        filetime::set_file_mtime(thread_dir.join(".jyc"), old_filetime).unwrap();

        let mut pattern = ChannelPattern::default();
        pattern.name = "developer".to_string();
        pattern.idle_cleanup = Some(crate::config::types::IdleCleanupConfig {
            enabled: true,
            timeout_secs: 86400,
            clean_paths: vec!["repo".to_string()],
            interval_secs: 300,
            skip_cleanup: false,
        });

        let patterns = vec![pattern];
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        start_idle_cleanup_sweep(workspace.to_path_buf(), patterns, cancel).await;

        assert!(thread_dir.join("repo").exists());
    }

    #[tokio::test]
    async fn test_idle_cleanup_skips_skip_flag() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        let thread_dir = workspace.join("skipped-thread");
        fs::create_dir_all(thread_dir.join(".jyc")).await.unwrap();
        fs::create_dir_all(thread_dir.join("repo")).await.unwrap();

        fs::write(thread_dir.join(".jyc").join("pattern"), "developer").await.unwrap();
        fs::write(thread_dir.join(".jyc").join("idle-cleanup-skip.flag"), "").await.unwrap();

        let old_time = SystemTime::now() - std::time::Duration::from_secs(86400 * 2);
        let old_filetime = filetime::FileTime::from_system_time(old_time);
        filetime::set_file_mtime(thread_dir.join(".jyc"), old_filetime).unwrap();

        let mut pattern = ChannelPattern::default();
        pattern.name = "developer".to_string();
        pattern.idle_cleanup = Some(crate::config::types::IdleCleanupConfig {
            enabled: true,
            timeout_secs: 86400,
            clean_paths: vec!["repo".to_string()],
            interval_secs: 300,
            skip_cleanup: false,
        });

        let patterns = vec![pattern];
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        start_idle_cleanup_sweep(workspace.to_path_buf(), patterns, cancel).await;

        assert!(thread_dir.join("repo").exists());
    }

    #[tokio::test]
    async fn test_idle_cleanup_removes_stale_directories() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        let thread_dir = workspace.join("stale-thread");
        fs::create_dir_all(thread_dir.join(".jyc")).await.unwrap();
        fs::create_dir_all(thread_dir.join("repo")).await.unwrap();

        fs::write(thread_dir.join(".jyc").join("pattern"), "developer").await.unwrap();

        let old_time = SystemTime::now() - std::time::Duration::from_secs(86400 * 2);
        let old_filetime = filetime::FileTime::from_system_time(old_time);
        filetime::set_file_mtime(thread_dir.join(".jyc"), old_filetime).unwrap();

        let mut pattern = ChannelPattern::default();
        pattern.name = "developer".to_string();
        pattern.idle_cleanup = Some(crate::config::types::IdleCleanupConfig {
            enabled: true,
            timeout_secs: 86400,
            clean_paths: vec!["repo".to_string()],
            interval_secs: 300,
            skip_cleanup: false,
        });

        let patterns = vec![pattern];
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        start_idle_cleanup_sweep(workspace.to_path_buf(), patterns, cancel).await;

        assert!(!thread_dir.join("repo").exists());
        assert!(thread_dir.join(".jyc").join("idle-cleaned.flag").exists());
    }

    #[tokio::test]
    async fn test_idle_cleanup_preserves_active_threads() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        let thread_dir = workspace.join("active-thread");
        fs::create_dir_all(thread_dir.join(".jyc")).await.unwrap();
        fs::create_dir_all(thread_dir.join("repo")).await.unwrap();

        fs::write(thread_dir.join(".jyc").join("pattern"), "developer").await.unwrap();

        let recent_time = SystemTime::now() - std::time::Duration::from_secs(60);
        let recent_filetime = filetime::FileTime::from_system_time(recent_time);
        filetime::set_file_mtime(thread_dir.join(".jyc"), recent_filetime).unwrap();

        let mut pattern = ChannelPattern::default();
        pattern.name = "developer".to_string();
        pattern.idle_cleanup = Some(crate::config::types::IdleCleanupConfig {
            enabled: true,
            timeout_secs: 86400,
            clean_paths: vec!["repo".to_string()],
            interval_secs: 300,
            skip_cleanup: false,
        });

        let patterns = vec![pattern];
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        start_idle_cleanup_sweep(workspace.to_path_buf(), patterns, cancel).await;

        assert!(thread_dir.join("repo").exists());
        assert!(!thread_dir.join(".jyc").join("idle-cleaned.flag").exists());
    }

    #[tokio::test]
    async fn test_idle_cleanup_flag_removed_when_active() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        let thread_dir = workspace.join("reactivated-thread");
        fs::create_dir_all(thread_dir.join(".jyc")).await.unwrap();
        fs::create_dir_all(thread_dir.join("repo")).await.unwrap();

        fs::write(thread_dir.join(".jyc").join("pattern"), "developer").await.unwrap();
        fs::write(thread_dir.join(".jyc").join("idle-cleaned.flag"), "").await.unwrap();

        let recent_time = SystemTime::now() - std::time::Duration::from_secs(60);
        let recent_filetime = filetime::FileTime::from_system_time(recent_time);
        filetime::set_file_mtime(thread_dir.join(".jyc"), recent_filetime).unwrap();

        let mut pattern = ChannelPattern::default();
        pattern.name = "developer".to_string();
        pattern.idle_cleanup = Some(crate::config::types::IdleCleanupConfig {
            enabled: true,
            timeout_secs: 86400,
            clean_paths: vec!["repo".to_string()],
            interval_secs: 300,
            skip_cleanup: false,
        });

        let patterns = vec![pattern];
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        start_idle_cleanup_sweep(workspace.to_path_buf(), patterns, cancel).await;

        assert!(thread_dir.join("repo").exists());
        assert!(!thread_dir.join(".jyc").join("idle-cleaned.flag").exists());
    }

    #[test]
    fn test_is_safe_path() {
        assert!(is_safe_path("repo"));
        assert!(is_safe_path("src"));
        assert!(is_safe_path("workspace"));
        assert!(!is_safe_path("../etc"));
        assert!(!is_safe_path("foo/../bar"));
        assert!(!is_safe_path("foo\\..\\bar"));
        assert!(!is_safe_path(".."));
        assert!(!is_safe_path("repo/../etc"));
    }
}
