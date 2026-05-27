//! Glob tool — find files by pattern.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// Glob tool — finds files matching a glob pattern.
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Supports patterns like '**/*.rs', 'src/**/*.ts', etc. \
         Returns matching file paths sorted by modification time."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match files against (e.g., '**/*.rs')"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in (default: working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let pattern = input
            .get("pattern")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' parameter"))?;

        let search_dir = input
            .get("path")
            .and_then(|p| p.as_str())
            .map(|p| {
                if std::path::Path::new(p).is_absolute() {
                    std::path::PathBuf::from(p)
                } else {
                    ctx.working_dir.join(p)
                }
            })
            .unwrap_or_else(|| ctx.working_dir.to_path_buf());

        // Security: only check boundary when an explicit search_dir is
        // provided (different from the default working_dir).
        if search_dir != ctx.working_dir {
            let display = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
            if let Err(msg) = ctx.check_path_boundary(display, &search_dir) {
                return Ok(ToolOutput::error(msg));
            }
        }

        // Build full glob pattern
        let full_pattern = search_dir.join(pattern).to_string_lossy().to_string();

        let matches: Vec<String> = glob::glob(&full_pattern)
            .map_err(|e| anyhow::anyhow!("Invalid glob pattern '{}': {e}", pattern))?
            .filter_map(|entry| entry.ok())
            .filter_map(|path| {
                path.strip_prefix(ctx.working_dir)
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
                    .or_else(|| Some(path.to_string_lossy().to_string()))
            })
            .collect();

        if matches.is_empty() {
            Ok(ToolOutput::success(format!(
                "No files matching '{}'",
                pattern
            )))
        } else {
            Ok(ToolOutput::success(format!(
                "{} file(s) found:\n{}",
                matches.len(),
                matches.join("\n")
            )))
        }
    }
}
