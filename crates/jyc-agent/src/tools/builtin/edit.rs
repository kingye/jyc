//! Edit tool — perform string replacement in files.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// Edit tool — performs exact string replacement in files.
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Perform exact string replacements in files. The oldString must match exactly \
         (including whitespace and indentation). If oldString is found multiple times, \
         the edit will fail — provide more context to make it unique."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences (default: false)"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let file_path = input
            .get("file_path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'file_path' parameter"))?;
        let old_string = input
            .get("old_string")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'old_string' parameter"))?;
        let new_string = input
            .get("new_string")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'new_string' parameter"))?;
        let replace_all = input
            .get("replace_all")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);

        let path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            ctx.working_dir.join(file_path)
        };

        // Security: ensure path is within working directory or write roots.
        if let Err(msg) = ctx.check_write_boundary(file_path, &path) {
            return Ok(ToolOutput::error(msg));
        }

        // Read current content
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {e}", file_path))?;

        if old_string == new_string {
            return Ok(ToolOutput::error("oldString and newString are identical"));
        }

        // Count occurrences
        let count = content.matches(old_string).count();

        if count == 0 {
            return Ok(ToolOutput::error(format!(
                "oldString not found in '{}'. Make sure the text matches exactly (including whitespace).",
                file_path
            )));
        }

        if count > 1 && !replace_all {
            return Ok(ToolOutput::error(format!(
                "Found {} matches for oldString in '{}'. Provide more context to make it unique, or set replace_all=true.",
                count, file_path
            )));
        }

        // Perform replacement
        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        // Calculate the starting line number of the first replacement.
        let line_no = content
            .find(old_string)
            .map(|pos| content[..pos].lines().count())
            .unwrap_or(0);

        tokio::fs::write(&path, &new_content)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {e}", file_path))?;

        let replacements = if replace_all { count } else { 1 };
        Ok(ToolOutput::success(format!(
            "Edited '{}' at line {}: {} replacement(s) made",
            file_path, line_no, replacements
        )))
    }
}
