//! Write tool — write file contents.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// Write tool — creates or overwrites a file.
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, or overwrites it if it does. \
         Creates parent directories as needed."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The path to the file to write (relative to working directory or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let file_path = input
            .get("file_path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' parameter"))?;
        let content = input
            .get("content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'content' parameter"))?;

        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };

        // Security: ensure path is within working directory.
        if let Err(msg) = ctx.check_path_boundary(file_path, &path) {
            return Ok(ToolOutput::error(msg));
        }

        // Create parent directories
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to create directories: {e}"))?;
        }

        tokio::fs::write(&path, content)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write file: {e}"))?;

        Ok(ToolOutput::success(format!(
            "File written: {} ({} bytes)",
            file_path,
            content.len()
        )))
    }
}
