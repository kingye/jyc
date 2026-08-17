use std::path::PathBuf;

use anyhow::{Context, Result};

use super::handler::CommandContext;
use crate::topic_manager::TopicManager;

/// Shared result of resolving pin/unpin context from a command context.
pub struct PinContext {
    pub config_path: PathBuf,
    pub topic_name: String,
    pub adhoc_path: PathBuf,
}

/// Build a `PinContext` from the command context, validating that this is a
/// websocket topic with a known config file path.
pub async fn build_pin_context(
    context: &CommandContext,
    topic_manager: &TopicManager,
) -> Result<PinContext> {
    let config_path = context.config_path.clone().context("config_path is None")?;

    anyhow::ensure!(
        context.channel_type == "websocket",
        "channel type '{}' is not websocket",
        context.channel_type
    );

    let topic_name = context
        .topic_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown-topic")
        .to_string();

    let adhoc_path = {
        let paths = topic_manager.topic_paths.lock().await;
        paths
            .get(&topic_name)
            .cloned()
            .unwrap_or_else(|| context.topic_path.clone())
    };

    Ok(PinContext {
        config_path,
        topic_name,
        adhoc_path,
    })
}

/// Append a new `[agents.<agent_name>]` section to the config file on disk,
/// pinning the ad-hoc topic so it survives a `jyc serve` restart.
///
/// Writes the new agents form (supersedes the legacy
/// `[[channels.<name>.patterns]]` pin format):
///
/// ```toml
/// # Added by /pin command
/// [agents.<agent_name>]
/// topic_path = "<topic_path>"
/// ```
pub async fn append_agent_to_config(
    config_path: &std::path::Path,
    agent_name: &str,
    topic_path: &std::path::Path,
) -> Result<()> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("failed to read config: {}", config_path.display()))?;

    let escaped_path = topic_path.to_string_lossy().replace('\\', "\\\\");
    let section = format!(
        "\n# Added by /pin command\n[agents.{agent_name}]\ntopic_path = \"{escaped_path}\"\n",
    );

    let mut new_raw = raw;
    new_raw.push_str(&section);

    tokio::fs::write(config_path, &new_raw)
        .await
        .with_context(|| format!("failed to write config: {}", config_path.display()))?;

    Ok(())
}

/// Remove a pinned section matching the given topic_path from the config
/// file. Handles both the new `[agents.<name>]` section (with a `topic_path`
/// field) and the legacy `[[channels.<name>.patterns]]` block.
/// Returns true if a section was removed.
pub async fn remove_pinned_from_config(
    config_path: &std::path::Path,
    topic_path: &std::path::Path,
) -> Result<bool> {
    let raw = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("failed to read config: {}", config_path.display()))?;

    let target = normalize_path_line(&topic_path.to_string_lossy());
    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    let mut i = 0;
    let mut removed = false;

    while i < lines.len() {
        if is_pinned_section_header(&lines[i])
            && is_section_matching(
                &lines[i..].iter().map(|l| l.as_str()).collect::<Vec<_>>(),
                &target,
            )
        {
            // Find the start and end of this pinned section
            let start = i;
            i += 1;
            while i < lines.len() && !lines[i].starts_with('[') {
                i += 1;
            }
            let end = i; // exclusive

            // Remove blank lines and the pin marker comment just before
            // the block.
            let mut remove_start = start;
            while remove_start > 0 {
                let prev = lines[remove_start - 1].trim();
                if prev.is_empty() || (prev.starts_with('#') && prev.contains("/pin")) {
                    remove_start -= 1;
                } else {
                    break;
                }
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

/// True when the line is a pinned-section header: a legacy
/// `[[channels.<name>.patterns]]` array-of-tables header, or a new
/// `[agents.<name>]` table header.
fn is_pinned_section_header(line: &str) -> bool {
    (line.starts_with("[[") && line.ends_with(".patterns]]"))
        || line.trim_start().starts_with("[agents.")
}

/// Check if a pinned section (starting at `lines[start]`) has a matching
/// topic_path. Skips the section header line, then scans for `topic_path = "..."`
/// until the next section boundary.
fn is_section_matching(lines: &[&str], target_path: &str) -> bool {
    // Always skip the first line (the section header — `[[...]]` or `[agents.x]`).
    for line in &lines[1..] {
        let trimmed = line.trim();
        if trimmed.starts_with("topic_path") {
            // Extract the value: topic_path = "..."
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_append_and_remove_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        // Write initial config
        tokio::fs::write(
            &config_path,
            r#"
[agents.jyc]
topic_path = "~/projects/jyc"
"#,
        )
        .await
        .unwrap();

        let tp = tmp.path().join("my-project");

        // Append a new agent
        append_agent_to_config(&config_path, "my-project", &tp)
            .await
            .unwrap();

        // Verify it was appended
        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(content.contains("[agents.my-project]"));
        assert!(content.contains(tp.to_string_lossy().as_ref()));

        // Remove it
        let removed = remove_pinned_from_config(&config_path, &tp).await.unwrap();
        assert!(removed);

        // Verify it's gone (including the pin marker comment)
        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(!content.contains("my-project"));
        assert!(content.contains("[agents.jyc]")); // Original agent survives
    }

    #[tokio::test]
    async fn test_remove_legacy_pattern_still_works() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let tp = tmp.path().join("my-project");

        // Legacy [[channels.x.patterns]] block with the pin marker comment.
        tokio::fs::write(
            &config_path,
            format!(
                r#"
# Added by /pin command
[[channels.my_ws.patterns]]
name = "my-project"
enabled = true
topic_path = "{}"
"#,
                tp.to_string_lossy()
            ),
        )
        .await
        .unwrap();

        let removed = remove_pinned_from_config(&config_path, &tp).await.unwrap();
        assert!(removed);

        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(!content.contains("my-project"));
        assert!(!content.contains("/pin"));
    }

    #[tokio::test]
    async fn test_remove_nonexistent_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        tokio::fs::write(
            &config_path,
            r#"
[agents.jyc]
topic_path = "~/projects/jyc"
"#,
        )
        .await
        .unwrap();

        let tp = tmp.path().join("nonexistent");
        let removed = remove_pinned_from_config(&config_path, &tp).await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_append_to_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        tokio::fs::write(&config_path, "").await.unwrap();

        let tp = tmp.path().join("test");
        append_agent_to_config(&config_path, "test", &tp)
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(content.contains("[agents.test]"));
        assert!(content.contains("topic_path"));
    }

    #[tokio::test]
    async fn test_remove_only_matching_section() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let tp1 = tmp.path().join("project-a");
        let tp2 = tmp.path().join("project-b");

        // Create config with two agents
        tokio::fs::write(
            &config_path,
            format!(
                r#"
[agents.a]
topic_path = "{}"

[agents.b]
topic_path = "{}"
"#,
                tp1.to_string_lossy(),
                tp2.to_string_lossy()
            ),
        )
        .await
        .unwrap();

        // Remove only the first agent
        let removed = remove_pinned_from_config(&config_path, &tp1).await.unwrap();
        assert!(removed);

        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(!content.contains("project-a"));
        assert!(content.contains("project-b"));
    }
}
