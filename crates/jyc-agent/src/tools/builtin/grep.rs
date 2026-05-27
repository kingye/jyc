//! Grep tool — search file contents with regex.

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use std::path::Path;

use super::super::{Tool, ToolContext, ToolOutput};

/// Maximum number of results to return.
const MAX_RESULTS: usize = 100;

/// Grep tool — searches file contents using regex.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents using regular expressions. Returns file paths and line numbers \
         with matching content. Use 'include' to filter by file extension."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: working directory)"
                },
                "include": {
                    "type": "string",
                    "description": "File pattern to include (e.g., '*.rs', '*.{ts,tsx}')"
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
                if Path::new(p).is_absolute() {
                    std::path::PathBuf::from(p)
                } else {
                    ctx.working_dir.join(p)
                }
            })
            .unwrap_or_else(|| ctx.working_dir.to_path_buf());

        let include_pattern = input.get("include").and_then(|i| i.as_str());

        // Security: only check boundary when an explicit search_dir is
        // provided (i.e. different from the default working_dir).
        if search_dir != ctx.working_dir {
            let display = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
            if let Err(msg) = ctx.check_path_boundary(display, &search_dir) {
                return Ok(ToolOutput::error(msg));
            }
        }

        let regex = Regex::new(pattern)
            .map_err(|e| anyhow::anyhow!("Invalid regex pattern '{}': {e}", pattern))?;

        // Use ripgrep-style search: walk directory, read files, match lines
        let mut results = Vec::new();
        let mut files_searched = 0;

        search_recursive(
            &search_dir,
            &regex,
            include_pattern,
            ctx.working_dir,
            &mut results,
            &mut files_searched,
        )?;

        if results.is_empty() {
            Ok(ToolOutput::success(format!(
                "No matches for '{}' (searched {} files)",
                pattern, files_searched
            )))
        } else {
            let total = results.len();
            results.truncate(MAX_RESULTS);
            let mut output = results.join("\n");
            if total > MAX_RESULTS {
                output.push_str(&format!(
                    "\n\n... ({} total matches, showing first {})",
                    total, MAX_RESULTS
                ));
            }
            Ok(ToolOutput::success(output))
        }
    }
}

fn search_recursive(
    dir: &Path,
    regex: &Regex,
    include_pattern: Option<&str>,
    working_dir: &Path,
    results: &mut Vec<String>,
    files_searched: &mut usize,
) -> Result<()> {
    let entries = std::fs::read_dir(dir)?;

    for entry in entries.flatten() {
        let path = entry.path();

        // Skip hidden directories and common non-source dirs
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && (name.starts_with('.')
                || name == "node_modules"
                || name == "target"
                || name == "vendor")
        {
            continue;
        }

        if path.is_dir() {
            search_recursive(
                &path,
                regex,
                include_pattern,
                working_dir,
                results,
                files_searched,
            )?;
        } else if path.is_file() {
            // Check include pattern
            if let Some(pattern) = include_pattern {
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !matches_include_pattern(file_name, pattern) {
                    continue;
                }
            }

            // Read and search file
            if let Ok(content) = std::fs::read_to_string(&path) {
                *files_searched += 1;
                let rel_path = path
                    .strip_prefix(working_dir)
                    .unwrap_or(&path)
                    .to_string_lossy();

                for (line_num, line) in content.lines().enumerate() {
                    if regex.is_match(line) {
                        results.push(format!("{}:{}: {}", rel_path, line_num + 1, line.trim()));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Simple include pattern matching (supports *.ext and *.{ext1,ext2}).
fn matches_include_pattern(filename: &str, pattern: &str) -> bool {
    if pattern.starts_with("*.{") && pattern.ends_with('}') {
        // Handle *.{rs,ts,tsx} format
        let extensions = &pattern[3..pattern.len() - 1];
        extensions
            .split(',')
            .any(|ext| filename.ends_with(&format!(".{}", ext.trim())))
    } else if let Some(ext) = pattern.strip_prefix("*.") {
        filename.ends_with(&format!(".{}", ext))
    } else {
        filename.contains(pattern)
    }
}
