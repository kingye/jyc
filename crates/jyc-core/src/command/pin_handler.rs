use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use toml_edit::{DocumentMut, InlineTable, Table, Value};
use tracing::instrument;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::thread_manager::ThreadManager;

/// `/pin` command — persist an ad-hoc websocket thread to config.toml.
///
/// Adds a `[[channels.<websocket_name>.patterns]]` entry with `thread_path`
/// pointing to the thread's custom path. If no websocket channel exists, creates
/// a new `[channels.<thread_name>]` of type `"websocket"` with the pattern.
pub struct PinCommandHandler {
    thread_manager: Arc<ThreadManager>,
}

impl PinCommandHandler {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }
}

#[async_trait]
impl CommandHandler for PinCommandHandler {
    fn name(&self) -> &str {
        "/pin"
    }

    fn description(&self) -> &str {
        "Pin this ad-hoc websocket thread to config.toml"
    }

    #[instrument(skip(self, context))]
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // 1. Verify we have a config path
        let config_path = match &context.config_path {
            Some(p) => p.clone(),
            None => {
                return Ok(CommandResult {
                    success: false,
                    message: "Config file path is unknown. Cannot pin.".into(),
                    error: Some("config_path is None".into()),
                });
            }
        };

        // 2. Verify this is a websocket thread
        if context.channel_type != "websocket" {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "Cannot pin: channel type '{}' is not a websocket. Only websocket channels can be pinned.",
                    context.channel_type
                ),
                error: Some("not a websocket channel".into()),
            });
        }

        // 3. Read the config file
        let content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) => {
                return Ok(CommandResult {
                    success: false,
                    message: format!("Failed to read config file: {}", e),
                    error: Some(e.to_string()),
                });
            }
        };
        let mut doc: DocumentMut = content.parse().context("failed to parse config.toml")?;

        // 4. Resolve the thread's custom path
        let thread_name = context
            .thread_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("adhoc");
        let adhoc_path = {
            let paths = self.thread_manager.thread_paths.lock().await;
            paths
                .get(thread_name)
                .cloned()
                .unwrap_or_else(|| context.thread_path.clone())
        };

        // Collect all websocket channels
        let ws_channels: Vec<String> = doc
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

        let channel_name: String;

        if ws_channels.is_empty() {
            // No websocket channel exists — create a new one
            channel_name = thread_name.to_string();

            // Check if thread_name already exists as a channel
            let channels = doc
                .entry("channels")
                .or_insert(Table::new().into())
                .as_table_mut()
                .context("channels is not a table")?;

            // Only add if not already present
            if !channels.contains_key(&channel_name) {
                let mut channel = Table::new();
                channel.insert("type", "websocket".into());
                channels.insert(&channel_name, channel.into());
            }

            // Add pattern
            add_pattern_to_channel(&mut doc, &channel_name, thread_name, &adhoc_path)?;
        } else {
            // Use the first existing websocket channel
            channel_name = ws_channels[0].clone();

            // Check if this thread already has a pinned pattern
            let patterns = doc
                .get("channels")
                .and_then(|c| c.get(&channel_name))
                .and_then(|ch| ch.get("patterns"))
                .and_then(|p| p.as_array());

            if let Some(patterns) = patterns {
                for pattern in patterns {
                    if let Some(p) = pattern.as_inline_table()
                        && let Some(path_val) = p.get("thread_path").and_then(|v| v.as_str())
                    {
                        let normalized = normalize_path(path_val);
                        if normalized == normalize_path(&adhoc_path.to_string_lossy()) {
                            return Ok(CommandResult {
                                success: true,
                                message: format!(
                                    "Thread '{}' is already pinned to channel '{}' with thread_path '{}'.",
                                    thread_name, channel_name, path_val
                                ),
                                error: None,
                            });
                        }
                    }
                }
            }

            // Add pattern to existing channel
            add_pattern_to_channel(&mut doc, &channel_name, thread_name, &adhoc_path)?;
        }

        // Write back to config file
        if let Err(e) = std::fs::write(&config_path, doc.to_string()) {
            return Ok(CommandResult {
                success: false,
                message: format!("Failed to write config file: {}", e),
                error: Some(e.to_string()),
            });
        }

        let display_path = adhoc_path.to_string_lossy();
        Ok(CommandResult {
            success: true,
            message: format!(
                "✅ Pinned thread '{}' to channel '{}' with thread_path '{}'.\n⚠️ Restart `jyc serve` for the change to take effect.",
                thread_name, channel_name, display_path
            ),
            error: None,
        })
    }
}

/// Add a pattern entry to `[[channels.<channel_name>.patterns]]` with thread_path.
fn add_pattern_to_channel(
    doc: &mut DocumentMut,
    channel_name: &str,
    pattern_name: &str,
    thread_path: &Path,
) -> Result<()> {
    let channels = doc
        .entry("channels")
        .or_insert(Table::new().into())
        .as_table_mut()
        .context("channels is not a table")?;

    let channel = channels
        .entry(channel_name)
        .or_insert(Table::new().into())
        .as_table_mut()
        .context("channel entry is not a table")?;

    // Get or create the patterns array
    let patterns = channel
        .entry("patterns")
        .or_insert(Value::Array(toml_edit::Array::new()).into())
        .as_array_mut()
        .context("patterns is not an array")?;

    // Build the pattern inline table
    let mut pattern = InlineTable::new();
    pattern.insert("name", pattern_name.into());
    pattern.insert("enabled", Value::from(true));
    pattern.insert(
        "thread_path",
        Value::from(thread_path.to_string_lossy().as_ref()),
    );

    patterns.push(pattern);
    Ok(())
}

/// Normalize a filesystem path for comparison.
pub fn normalize_path(path: &str) -> String {
    let p = Path::new(path);
    let p = if p.is_relative() {
        // Resolve relative paths against current dir
        std::env::current_dir().unwrap_or_default().join(p)
    } else {
        p.to_path_buf()
    };
    // Canonicalize if possible, otherwise just trim trailing slashes
    std::fs::canonicalize(&p)
        .unwrap_or(p)
        .to_string_lossy()
        .to_string()
}
