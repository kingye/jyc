use std::path::PathBuf;

use anyhow::{Context, Result};
use toml_edit::DocumentMut;

use super::handler::CommandContext;
use crate::thread_manager::ThreadManager;

/// Shared result of resolving pin/unpin context from a command context.
pub struct PinContext {
    pub config_path: PathBuf,
    pub thread_name: String,
    pub adhoc_path: PathBuf,
    pub doc: DocumentMut,
    pub ws_channels: Vec<String>,
}

/// Build a `PinContext` from the command context, validating that this is a
/// websocket thread with a known config file path.
pub async fn build_pin_context(
    context: &CommandContext,
    thread_manager: &ThreadManager,
) -> Result<PinContext> {
    let config_path = context.config_path.clone().context("config_path is None")?;

    anyhow::ensure!(
        context.channel_type == "websocket",
        "channel type '{}' is not websocket",
        context.channel_type
    );

    let thread_name = context
        .thread_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown-thread")
        .to_string();

    let adhoc_path = {
        let paths = thread_manager.thread_paths.lock().await;
        paths
            .get(&thread_name)
            .cloned()
            .unwrap_or_else(|| context.thread_path.clone())
    };

    let content = tokio::fs::read_to_string(&config_path)
        .await
        .with_context(|| format!("failed to read config file: {}", config_path.display()))?;
    let doc: DocumentMut = content.parse().context("failed to parse config.toml")?;

    let ws_channels = doc
        .get("channels")
        .and_then(|c| c.as_table())
        .map(|tbl| {
            tbl.iter()
                .filter(|(_, v)| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t == "websocket")
                })
                .map(|(name, _)| name.to_string())
                .collect()
        })
        .unwrap_or_default();

    Ok(PinContext {
        config_path,
        thread_name,
        adhoc_path,
        doc,
        ws_channels,
    })
}

/// Append a new `[[channels.<channel_name>.patterns]]` section to the config
/// file on disk. Returns the index of the new pattern (0-based).
pub async fn append_pattern_to_config(
    config_path: &std::path::Path,
    channel_name: &str,
    pattern_name: &str,
    thread_path: &std::path::Path,
) -> Result<()> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("failed to read config: {}", config_path.display()))?;

    // Create the new TOML section
    let escaped_path = thread_path.to_string_lossy().replace('\\', "\\\\");
    let section = format!(
        "\n# Added by /pin command\n[[channels.{}.patterns]]\nname = \"{}\"\nenabled = true\nthread_path = \"{}\"\n\n[channels.{}.patterns.rules]\n",
        channel_name, pattern_name, escaped_path, channel_name
    );

    let mut new_raw = raw;
    new_raw.push_str(&section);

    tokio::fs::write(config_path, &new_raw)
        .await
        .with_context(|| format!("failed to write config: {}", config_path.display()))?;

    Ok(())
}

/// Remove a pattern section matching the given thread_path from the config file.
/// Returns true if a section was removed.
pub async fn remove_pattern_from_config(
    config_path: &std::path::Path,
    thread_path: &std::path::Path,
) -> Result<bool> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("failed to read config: {}", config_path.display()))?;

    let target = normalize_path_line(&thread_path.to_string_lossy());
    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    let mut i = 0;
    let mut removed = false;

    while i < lines.len() {
        if lines[i].starts_with("[[")
            && lines[i].ends_with(".patterns]]")
            && is_pattern_matching(
                &lines[i..].iter().map(|l| l.as_str()).collect::<Vec<_>>(),
                &target,
            )
        {
            // Find the start and end of this pattern block
            let start = i;
            i += 1;
            while i < lines.len() && !lines[i].starts_with('[') {
                i += 1;
            }
            let end = i; // exclusive

            // Remove blank lines just before the block
            let mut remove_start = start;
            while remove_start > 0 && lines[remove_start - 1].trim().is_empty() {
                remove_start -= 1;
            }

            // Mark all lines in range as removed
            for line in &mut lines[remove_start..end] {
                line.clear();
            }

            removed = true;
            break;
        }
        i += 1;
    }

    if removed {
        // Rebuild: filter out removed lines
        let new_raw = lines
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        tokio::fs::write(config_path, &new_raw)
            .await
            .with_context(|| format!("failed to write config: {}", config_path.display()))?;
    }

    Ok(removed)
}

/// Check if a pattern section (starting at `lines[start]`) has a matching thread_path.
fn is_pattern_matching(lines: &[&str], target_path: &str) -> bool {
    // Skip the first line (the section header like `[[...]]`)
    let start = if lines
        .first()
        .is_some_and(|l| l.trim_start().starts_with("[["))
    {
        1
    } else {
        0
    };
    for line in &lines[start..] {
        let trimmed = line.trim();
        if trimmed.starts_with("thread_path") {
            // Extract the value: thread_path = "..."
            if let Some(val_start) = trimmed.find('"')
                && let Some(val_end) = trimmed[val_start + 1..].find('"')
            {
                let path_val = &trimmed[val_start + 1..val_start + 1 + val_end];
                if normalize_path_line(path_val) == target_path {
                    return true;
                }
            }
        }
        // Stop at next section boundary (a line starting with `[`)
        if trimmed.starts_with('[') {
            break;
        }
    }
    false
}

/// Normalize a path for config line comparison.
fn normalize_path_line(path: &str) -> String {
    let p = std::path::Path::new(path);
    let p = if p.is_relative() {
        std::env::current_dir().unwrap_or_default().join(p)
    } else {
        p.to_path_buf()
    };
    std::fs::canonicalize(&p)
        .unwrap_or(p)
        .to_string_lossy()
        .to_string()
}

/// Write the TOML document back to the config file.
pub async fn write_config(config_path: &std::path::Path, doc: &DocumentMut) -> Result<()> {
    tokio::fs::write(config_path, doc.to_string())
        .await
        .with_context(|| format!("failed to write config file: {}", config_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_append_and_remove_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        // Write initial config
        tokio::fs::write(
            &config_path,
            r#"
[channels.my_ws]
type = "websocket"

[[channels.my_ws.patterns]]
name = "general"
enabled = true
"#,
        )
        .await
        .unwrap();

        let tp = tmp.path().join("my-project");

        // Append a new pattern
        append_pattern_to_config(&config_path, "my_ws", "my-project", &tp)
            .await
            .unwrap();

        // Verify it was appended
        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(content.contains("name = \"my-project\""));
        assert!(content.contains(tp.to_string_lossy().as_ref()));

        // Remove it
        let removed = remove_pattern_from_config(&config_path, &tp).await.unwrap();
        assert!(removed);

        // Verify it's gone
        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(!content.contains("my-project"));
        assert!(content.contains("general")); // Original pattern survives
    }

    #[tokio::test]
    async fn test_remove_nonexistent_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        tokio::fs::write(
            &config_path,
            r#"
[[channels.my_ws.patterns]]
name = "general"
enabled = true
"#,
        )
        .await
        .unwrap();

        let tp = tmp.path().join("nonexistent");
        let removed = remove_pattern_from_config(&config_path, &tp).await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_append_to_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        tokio::fs::write(&config_path, "").await.unwrap();

        let tp = tmp.path().join("test");
        append_pattern_to_config(&config_path, "my_ws", "test", &tp)
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(content.contains("thread_path"));
    }

    #[tokio::test]
    async fn test_remove_only_matching_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let tp1 = tmp.path().join("project-a");
        let tp2 = tmp.path().join("project-b");

        // Create config with two patterns
        tokio::fs::write(
            &config_path,
            format!(
                r#"
[channels.my_ws]
type = "websocket"

[[channels.my_ws.patterns]]
name = "a"
thread_path = "{}"

[[channels.my_ws.patterns]]
name = "b"
thread_path = "{}"
"#,
                tp1.to_string_lossy(),
                tp2.to_string_lossy()
            ),
        )
        .await
        .unwrap();

        // Remove only the first pattern
        let removed = remove_pattern_from_config(&config_path, &tp1)
            .await
            .unwrap();
        assert!(removed);

        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(!content.contains("project-a"));
        assert!(content.contains("project-b"));
    }
}
