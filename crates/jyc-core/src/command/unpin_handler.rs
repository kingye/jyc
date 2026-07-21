use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::instrument;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::pin_handler::normalize_path;
use crate::thread_manager::ThreadManager;

/// `/unpin` command — remove a pinned thread configuration from config.toml.
///
/// Finds the `[[channels.<websocket_name>.patterns]]` entry whose `thread_path`
/// matches the current thread's path and removes it.
pub struct UnpinCommandHandler {
    thread_manager: Arc<ThreadManager>,
}

impl UnpinCommandHandler {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }
}

#[async_trait]
impl CommandHandler for UnpinCommandHandler {
    fn name(&self) -> &str {
        "/unpin"
    }

    fn description(&self) -> &str {
        "Remove pinned thread configuration from config.toml"
    }

    #[instrument(skip(self, context))]
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // 1. Verify we have a config path
        let config_path = match &context.config_path {
            Some(p) => p.clone(),
            None => {
                return Ok(CommandResult {
                    success: false,
                    message: "Config file path is unknown. Cannot unpin.".into(),
                    error: Some("config_path is None".into()),
                });
            }
        };

        // 2. Verify this is a websocket thread
        if context.channel_type != "websocket" {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "Cannot unpin: channel type '{}' is not a websocket.",
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
        let mut doc: toml_edit::DocumentMut =
            content.parse().context("failed to parse config.toml")?;

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

        let target_path = normalize_path(&adhoc_path.to_string_lossy());

        // 5. Find and remove matching pattern from each websocket channel
        let channels = doc
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
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if channels.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: "No websocket channels found in config. Nothing to unpin.".into(),
                error: Some("no websocket channels".into()),
            });
        }

        let mut removed = false;
        let mut removed_from = String::new();
        let mut pattern_name = String::new();

        for ch_name in &channels {
            // Access channels.<ch_name>.patterns via mutable index
            if let Some(patterns_array) = doc
                .get_mut("channels")
                .and_then(|c| c.as_table_mut())
                .and_then(|c| c.get_mut(ch_name))
                .and_then(|ch| ch.as_table_mut())
                .and_then(|ch| ch.get_mut("patterns"))
                .and_then(|p| p.as_array_mut())
            {
                let mut i = 0;
                while i < patterns_array.len() {
                    // Check if this pattern matches (read-only borrow)
                    let should_remove = patterns_array
                        .get(i)
                        .and_then(|v| v.as_inline_table())
                        .is_some_and(|p| {
                            p.get("thread_path")
                                .and_then(|v| v.as_str())
                                .is_some_and(|path_val| normalize_path(path_val) == target_path)
                        });

                    if should_remove {
                        // Capture name before removing
                        if let Some(val) = patterns_array.get(i)
                            && let Some(p) = val.as_inline_table()
                            && let Some(name_val) = p.get("name").and_then(|v| v.as_str())
                        {
                            pattern_name = name_val.to_string();
                        }
                        patterns_array.remove(i);
                        removed = true;
                        removed_from = ch_name.clone();
                        continue; // Don't increment i
                    }
                    i += 1;
                }
            }
        }

        if !removed {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "Thread '{}' is not pinned. No matching pattern found in any websocket channel.",
                    thread_name
                ),
                error: Some("pattern not found".into()),
            });
        }

        // 6. Write back to config file
        if let Err(e) = std::fs::write(&config_path, doc.to_string()) {
            return Ok(CommandResult {
                success: false,
                message: format!("Failed to write config file: {}", e),
                error: Some(e.to_string()),
            });
        }

        Ok(CommandResult {
            success: true,
            message: format!(
                "✅ Unpinned pattern '{}' from channel '{}'.\n⚠️ Restart `jyc serve` for the change to take effect.",
                pattern_name, removed_from
            ),
            error: None,
        })
    }
}
