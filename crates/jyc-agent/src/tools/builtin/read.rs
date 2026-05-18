//! Read tool — read file contents.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// Maximum number of lines to return by default.
const DEFAULT_LIMIT: usize = 2000;

/// Read tool — reads file or directory contents.
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file or directory from the filesystem. Returns file contents with line numbers, \
         or directory listing. Use offset and limit to read specific sections of large files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The path to the file or directory to read (relative to working directory or absolute)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start from (1-indexed, default: 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default: 2000)"
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let file_path = input.get("file_path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' parameter"))?;

        let offset = input.get("offset")
            .and_then(|o| o.as_u64())
            .unwrap_or(1) as usize;
        let limit = input.get("limit")
            .and_then(|l| l.as_u64())
            .unwrap_or(DEFAULT_LIMIT as u64) as usize;

        // Resolve path relative to working directory
        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };

        // Check the path exists
        if !path.exists() {
            return Ok(ToolOutput::error(format!(
                "Path not found: '{}'", file_path
            )));
        }

        // Security: ensure path is within working directory.
        // Skip this check if path traverses a symlink (e.g., repo/ -> /other/path),
        // since JYC's repo_group feature uses symlinks within the working directory.
        let has_symlink = path.ancestors().any(|ancestor| {
            ancestor != ctx.working_dir && ancestor.is_symlink()
        });

        if !has_symlink {
            let canonical = path.canonicalize()
                .unwrap_or_else(|_| path.clone());
            let working_canonical = ctx.working_dir.canonicalize()
                .unwrap_or_else(|_| ctx.working_dir.to_path_buf());

            if !canonical.starts_with(&working_canonical) {
                return Ok(ToolOutput::error(format!(
                    "Access denied: path '{}' is outside the working directory", file_path
                )));
            }
        }

        if path.is_dir() {
            // Read directory listing
            let mut entries = Vec::new();
            let mut read_dir = tokio::fs::read_dir(&path).await
                .map_err(|e| anyhow::anyhow!("Failed to read directory: {e}"))?;

            while let Some(entry) = read_dir.next_entry().await? {
                let name = entry.file_name().to_string_lossy().to_string();
                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    entries.push(format!("{}/", name));
                } else {
                    entries.push(name);
                }
            }

            entries.sort();
            Ok(ToolOutput::success(entries.join("\n")))
        } else {
            // Read file contents
            let content = tokio::fs::read_to_string(&path).await
                .map_err(|e| anyhow::anyhow!("Failed to read file: {e}"))?;

            let lines: Vec<&str> = content.lines().collect();
            let start = offset.saturating_sub(1).min(lines.len());
            let end = (start + limit).min(lines.len());

            let mut result = String::new();
            for (i, line) in lines[start..end].iter().enumerate() {
                result.push_str(&format!("{}: {}\n", start + i + 1, line));
            }

            if end < lines.len() {
                result.push_str(&format!(
                    "\n(Showing lines {}-{} of {}. Use offset={} to continue.)",
                    start + 1, end, lines.len(), end + 1
                ));
            }

            Ok(ToolOutput::success(result))
        }
    }
}
