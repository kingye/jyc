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
        "Search file contents using regular expressions. Returns file paths and line \
         numbers with matching content."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search (default: working directory)"
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

        let search_path = input
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
        let explicit_path = input.get("path").and_then(|p| p.as_str());
        let display_path = explicit_path.unwrap_or("");

        // Security: only check boundary when an explicit search_path is
        // provided (i.e. different from the default working_dir).
        if search_path != ctx.working_dir
            && let Err(msg) = ctx.check_path_boundary(display_path, &search_path)
        {
            return Ok(ToolOutput::error(msg));
        }

        if explicit_path.is_some() && !search_path.exists() {
            return Ok(ToolOutput::error(format!(
                "Path not found: '{display_path}'"
            )));
        }

        let regex = Regex::new(pattern)
            .map_err(|e| anyhow::anyhow!("Invalid regex pattern '{}': {e}", pattern))?;

        // Search one explicit file or walk the directory recursively.
        let mut results = Vec::new();
        let mut files_searched = 0;

        if search_path.is_file() {
            if let Err(e) = search_file(
                &search_path,
                &regex,
                include_pattern,
                ctx.working_dir,
                &mut results,
                &mut files_searched,
            ) {
                return Ok(ToolOutput::error(format!(
                    "Failed to read file '{display_path}': {e}"
                )));
            }
        } else {
            search_recursive(
                &search_path,
                &regex,
                include_pattern,
                ctx.working_dir,
                &mut results,
                &mut files_searched,
            )?;
        }

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
        } else if path.is_file()
            && let Err(error) = search_file(
                &path,
                regex,
                include_pattern,
                working_dir,
                results,
                files_searched,
            )
        {
            tracing::debug!(path = %path.display(), %error, "skipping unreadable file");
        }
    }

    Ok(())
}

fn search_file(
    path: &Path,
    regex: &Regex,
    include_pattern: Option<&str>,
    working_dir: &Path,
    results: &mut Vec<String>,
    files_searched: &mut usize,
) -> std::io::Result<()> {
    if let Some(pattern) = include_pattern {
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !matches_include_pattern(file_name, pattern) {
            return Ok(());
        }
    }

    let content = std::fs::read_to_string(path)?;
    *files_searched += 1;
    let rel_path = path
        .strip_prefix(working_dir)
        .unwrap_or(path)
        .to_string_lossy();

    for (line_num, line) in content.lines().enumerate() {
        if regex.is_match(line) {
            results.push(format!("{}:{}: {}", rel_path, line_num + 1, line.trim()));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(tmp: &tempfile::TempDir) {
        std::fs::create_dir(tmp.path().join("docs")).unwrap();
        std::fs::write(tmp.path().join("notes.md"), "alpha\nneedle one\nneedle two").unwrap();
        std::fs::write(tmp.path().join("sibling.md"), "needle sibling").unwrap();
        std::fs::write(tmp.path().join("docs/code.rs"), "needle code").unwrap();
    }

    #[tokio::test]
    async fn explicit_file_path_searches_only_that_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(&tmp);
        let absolute = tmp.path().join("notes.md");

        for path in ["notes.md", absolute.to_str().unwrap()] {
            let out = GrepTool
                .execute(
                    json!({"pattern": "needle", "path": path}),
                    &ToolContext::new(tmp.path()),
                )
                .await
                .unwrap();

            assert_eq!(
                out.content,
                "notes.md:2: needle one\nnotes.md:3: needle two"
            );
        }
    }

    #[tokio::test]
    async fn explicit_file_path_honors_include_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(&tmp);
        let ctx = ToolContext::new(tmp.path());

        let excluded = GrepTool
            .execute(
                json!({"pattern": "needle", "path": "notes.md", "include": "*.rs"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            excluded.content,
            "No matches for 'needle' (searched 0 files)"
        );

        let included = GrepTool
            .execute(
                json!({"pattern": "needle", "path": "notes.md", "include": "*.md"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            included.content,
            "notes.md:2: needle one\nnotes.md:3: needle two"
        );
    }

    #[tokio::test]
    async fn missing_explicit_path_reports_a_named_error() {
        let tmp = tempfile::tempdir().unwrap();

        let out = GrepTool
            .execute(
                json!({"pattern": "needle", "path": "missing.md"}),
                &ToolContext::new(tmp.path()),
            )
            .await
            .unwrap();

        assert!(out.is_error);
        assert_eq!(out.content, "Path not found: 'missing.md'");
    }

    #[tokio::test]
    async fn unreadable_explicit_file_reports_a_named_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("binary.log"), b"needle \xff").unwrap();

        let out = GrepTool
            .execute(
                json!({"pattern": "needle", "path": "binary.log"}),
                &ToolContext::new(tmp.path()),
            )
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(
            out.content.starts_with("Failed to read file 'binary.log':"),
            "unexpected output: {}",
            out.content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_file_path_still_enforces_the_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("escape")).unwrap();

        let out = GrepTool
            .execute(
                json!({"pattern": "root", "path": "escape"}),
                &ToolContext::new(tmp.path()),
            )
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(
            out.content.contains("Access denied"),
            "unexpected output: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn directory_search_still_honors_include_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(&tmp);

        let out = GrepTool
            .execute(
                json!({"pattern": "needle", "include": "*.rs"}),
                &ToolContext::new(tmp.path()),
            )
            .await
            .unwrap();

        assert_eq!(out.content, "docs/code.rs:1: needle code");
    }
}
