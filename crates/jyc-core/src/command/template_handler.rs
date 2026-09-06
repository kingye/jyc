use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::template_utils::{copy_template_files, overwrite_template_files};

pub struct TemplateCommandHandler;

#[async_trait]
impl CommandHandler for TemplateCommandHandler {
    fn name(&self) -> &str {
        "/template"
    }

    fn description(&self) -> &str {
        "Manage topic templates. Subcommands: update (overwrite existing files)"
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
        let topic_path = &context.topic_path;

        let pattern_file = topic_path.join(".jyc").join("pattern");
        let pattern_name = if pattern_file.exists() {
            tokio::fs::read_to_string(&pattern_file)
                .await?
                .trim()
                .to_string()
        } else {
            return Ok(CommandResult {
                success: false,
                message: "/template: pattern file not found. Cannot determine template.".into(),
                error: Some("No .jyc/pattern file".to_string()),
                append_body: None,
            });
        };

        let template_name = context
            .config
            .channels
            .values()
            .flat_map(|c| c.patterns.iter().flatten())
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.template.clone());

        let template_name = match template_name {
            Some(t) => t,
            None => {
                return Ok(CommandResult {
                    success: false,
                    message: format!(
                        "/template: pattern '{}' has no template configured",
                        pattern_name
                    ),
                    error: Some("No template in pattern config".to_string()),
                    append_body: None,
                });
            }
        };

        let Some(template_src) = context
            .template_dirs
            .resolve_with_topic(&context.topic_path, &template_name)
        else {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "/template: template '{}' not found in any templates layer",
                    template_name
                ),
                error: Some(format!("Template not found: {}", template_name)),
                append_body: None,
            });
        };

        let copied = copy_template_files(&template_src, topic_path).await?;

        Ok(CommandResult {
            success: true,
            message: format!(
                "/template: applied '{}' template ({} files copied, existing files skipped)",
                template_name, copied
            ),
            error: None,
            append_body: None,
        })
    }

    /// `/template update` — re-apply template, overwrite existing files
    async fn execute_update(&self, context: &CommandContext) -> Result<CommandResult> {
        let topic_path = &context.topic_path;

        let pattern_file = topic_path.join(".jyc").join("pattern");
        let pattern_name = if pattern_file.exists() {
            tokio::fs::read_to_string(&pattern_file)
                .await?
                .trim()
                .to_string()
        } else {
            return Ok(CommandResult {
                success: false,
                message: "/template update: pattern file not found. Cannot determine template."
                    .into(),
                error: Some("No .jyc/pattern file".to_string()),
                append_body: None,
            });
        };

        tracing::debug!(pattern = %pattern_name, "Looking up template for pattern");

        // Debug: log all available patterns
        let all_patterns: Vec<_> = context
            .config
            .channels
            .iter()
            .flat_map(|(ch, c)| {
                c.patterns
                    .iter()
                    .flatten()
                    .map(move |p| format!("{}:{} template={:?}", ch, p.name, p.template))
            })
            .collect();
        tracing::debug!(patterns = ?all_patterns, "Available patterns in config");

        let template_name = context
            .config
            .channels
            .values()
            .flat_map(|c| c.patterns.iter().flatten())
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.template.clone());

        let template_name = match template_name {
            Some(t) => t,
            None => {
                return Ok(CommandResult {
                    success: false,
                    message: format!(
                        "/template update: pattern '{}' has no template configured",
                        pattern_name
                    ),
                    error: Some("No template in pattern config".to_string()),
                    append_body: None,
                });
            }
        };

        let Some(template_src) = context
            .template_dirs
            .resolve_with_topic(&context.topic_path, &template_name)
        else {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "/template update: template '{}' not found in any templates layer",
                    template_name
                ),
                error: Some(format!("Template not found: {}", template_name)),
                append_body: None,
            });
        };

        let copied = overwrite_template_files(&template_src, topic_path).await?;

        Ok(CommandResult {
            success: true,
            message: format!(
                "/template update: applied '{}' template ({} files overwritten)",
                template_name, copied
            ),
            error: None,
            append_body: None,
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
            topic_path: tmp_dir.to_path_buf(),
            config: Arc::new(
                jyc_types::load_config_from_str(
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
mode = "agent"
"#,
                )
                .unwrap(),
            ),
            channel: "test".into(),
            agent: None,
            template_dirs: tmp_dir.join("templates").into(),
            channel_type: "websocket".to_string(),
            config_path: None,
            per_agent_commands: vec![],
        }
    }

    #[tokio::test]
    async fn test_template_no_pattern_file() {
        let tmp = tempfile::tempdir().unwrap();

        // Create empty topic dir (no .jyc/pattern)
        let topic_dir = tmp.path().join("topic1");
        tokio::fs::create_dir_all(&topic_dir).await.unwrap();

        let handler = TemplateCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.topic_path = topic_dir;

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
        tokio::fs::write(template_src.join("test.txt"), "test content")
            .await
            .unwrap();

        // Verify template file exists
        println!("Template src: {:?}", template_src);
        println!(
            "Template file exists: {}",
            template_src.join("test.txt").exists()
        );

        // Create topic dir with .jyc/pattern file
        let topic_dir = tmp.path().join("topic1");
        let jyc_dir = topic_dir.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("pattern"), "test_pattern")
            .await
            .unwrap();

        let handler = TemplateCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.topic_path = topic_dir.clone();

        println!("Template dir in ctx: {:?}", ctx.template_dirs);

        let result = handler.execute(ctx).await.unwrap();

        println!(
            "Result: success={}, message={}",
            result.success, result.message
        );
        println!("Topic dir: {:?}", topic_dir);
        println!("Test file exists: {}", topic_dir.join("test.txt").exists());

        assert!(
            result.success,
            "Result should be success: {}",
            result.message
        );
        assert!(result.message.contains("test_template"));
        // Template files go to topic root, not .jyc/
        assert!(
            topic_dir.join("test.txt").exists(),
            "Template file should be copied to topic dir"
        );

        // Also verify .jyc/pattern is preserved
        assert!(topic_dir.join(".jyc").join("pattern").exists());
    }
}
