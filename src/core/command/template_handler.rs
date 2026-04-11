use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::core::template_utils::{copy_template_files, overwrite_template_files};

pub struct TemplateCommandHandler;

#[async_trait]
impl CommandHandler for TemplateCommandHandler {
    fn name(&self) -> &str {
        "/template"
    }

    fn description(&self) -> &str {
        "Manage thread templates. Subcommands: update (overwrite existing files)"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let subcommand = context.args.first().map(|s| s.as_str());
        
        match subcommand {
            Some("update") => self.execute_update(&context).await,
            _ => self.execute_apply(&context).await,
        }
    }
}

impl TemplateCommandHandler {
    /// `/template` (no subcommand) — apply template, skip existing files
    async fn execute_apply(&self, context: &CommandContext) -> Result<CommandResult> {
        let thread_path = &context.thread_path;
        
        let pattern_file = thread_path.join(".jyc").join("pattern");
        let pattern_name = if pattern_file.exists() {
            tokio::fs::read_to_string(&pattern_file).await?.trim().to_string()
        } else {
            return Ok(CommandResult {
                success: false,
                message: "/template: pattern file not found. Cannot determine template.".into(),
                error: Some("No .jyc/pattern file".to_string()),
                requires_restart: false,
            });
        };
        
        let template_name = context.config.channels
            .values()
            .flat_map(|c| c.patterns.iter().flatten())
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.template.clone());
        
        let template_name = match template_name {
            Some(t) => t,
            None => {
                return Ok(CommandResult {
                    success: false,
                    message: format!("/template: pattern '{}' has no template configured", pattern_name),
                    error: Some("No template in pattern config".to_string()),
                    requires_restart: false,
                });
            }
        };
        
        let template_src = context.template_dir.join(&template_name);
        if !template_src.exists() {
            return Ok(CommandResult {
                success: false,
                message: format!("/template: template '{}' not found in templates/", template_name),
                error: Some(format!("Path does not exist: {}", template_src.display())),
                requires_restart: false,
            });
        }
        
        let copied = copy_template_files(&template_src, thread_path).await?;
        
        Ok(CommandResult {
            success: true,
            message: format!("/template: applied '{}' template ({} files copied, existing files skipped)", template_name, copied),
            error: None,
            requires_restart: false,
        })
    }

    /// `/template update` — re-apply template, overwrite existing files
    async fn execute_update(&self, context: &CommandContext) -> Result<CommandResult> {
        let thread_path = &context.thread_path;
        
        let pattern_file = thread_path.join(".jyc").join("pattern");
        let pattern_name = if pattern_file.exists() {
            tokio::fs::read_to_string(&pattern_file).await?.trim().to_string()
        } else {
            return Ok(CommandResult {
                success: false,
                message: "/template update: pattern file not found. Cannot determine template.".into(),
                error: Some("No .jyc/pattern file".to_string()),
                requires_restart: false,
            });
        };
        
        tracing::debug!(pattern = %pattern_name, "Looking up template for pattern");
        
        let template_name = context.config.channels
            .values()
            .flat_map(|c| c.patterns.iter().flatten())
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.template.clone());
        
        let template_name = match template_name {
            Some(t) => t,
            None => {
                return Ok(CommandResult {
                    success: false,
                    message: format!("/template update: pattern '{}' has no template configured", pattern_name),
                    error: Some("No template in pattern config".to_string()),
                    requires_restart: false,
                });
            }
        };
        
        let template_src = context.template_dir.join(&template_name);
        if !template_src.exists() {
            return Ok(CommandResult {
                success: false,
                message: format!("/template update: template '{}' not found in templates/", template_name),
                error: Some(format!("Path does not exist: {}", template_src.display())),
                requires_restart: false,
            });
        }
        
        let copied = overwrite_template_files(&template_src, thread_path).await?;
        
        Ok(CommandResult {
            success: true,
            message: format!("/template update: applied '{}' template ({} files overwritten)", template_name, copied),
            error: None,
            requires_restart: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    fn test_context(tmp_dir: &Path) -> CommandContext {
        CommandContext {
            args: vec![],
            thread_path: tmp_dir.to_path_buf(),
            config: Arc::new(crate::config::load_config_from_str(
                r#"
[general]
[channels.test]
type = "email"
[[channels.test.patterns]]
name = "test_pattern"
template = "test_template"
[channels.test.patterns.rules]
sender = { email = "test@example.com" }

[agent]
enabled = true
mode = "opencode"
"#
            ).unwrap()),
            channel: "test".into(),
            agent: None,
            template_dir: tmp_dir.join("templates"),
        }
    }

    #[tokio::test]
    async fn test_template_no_pattern_file() {
        let tmp = tempfile::tempdir().unwrap();
        
        // Create empty thread dir (no .jyc/pattern)
        let thread_dir = tmp.path().join("thread1");
        tokio::fs::create_dir_all(&thread_dir).await.unwrap();
        
        let handler = TemplateCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.thread_path = thread_dir;
        
        let result = handler.execute(ctx).await.unwrap();
        
        assert!(!result.success);
        assert!(result.message.contains("pattern file not found"));
    }

    #[tokio::test]
    async fn test_template_success() {
        let tmp = tempfile::tempdir().unwrap();
        
        // Create template directory in templates/
        let template_src = tmp.path().join("templates").join("test_template");
        tokio::fs::create_dir_all(&template_src).await.unwrap();
        tokio::fs::write(template_src.join("test.txt"), "test content").await.unwrap();
        
        // Verify template file exists
        println!("Template src: {:?}", template_src);
        println!("Template file exists: {}", template_src.join("test.txt").exists());
        
        // Create thread dir with .jyc/pattern file
        let thread_dir = tmp.path().join("thread1");
        let jyc_dir = thread_dir.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("pattern"), "test_pattern").await.unwrap();
        
        let handler = TemplateCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.thread_path = thread_dir.clone();
        
        println!("Template dir in ctx: {:?}", ctx.template_dir);
        
        let result = handler.execute(ctx).await.unwrap();
        
        println!("Result: success={}, message={}", result.success, result.message);
        println!("Thread dir: {:?}", thread_dir);
        println!("Test file exists: {}", thread_dir.join("test.txt").exists());
        
        assert!(result.success, "Result should be success: {}", result.message);
        assert!(result.message.contains("test_template"));
        // Template files go to thread root, not .jyc/
        assert!(thread_dir.join("test.txt").exists(), "Template file should be copied to thread dir");
        
        // Also verify .jyc/pattern is preserved
        assert!(thread_dir.join(".jyc").join("pattern").exists());
    }
}