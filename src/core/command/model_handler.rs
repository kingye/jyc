use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::services::opencode::client::OpenCodeClient;
use crate::services::opencode::types::{Model, ProvidersResponse};

/// /model command — switch AI model for this thread.
///
/// Usage:
///   /model <model-id>    Switch to a specific model
///   /model reset          Reset to default model from config
///   /model                Show current model and list available models
pub struct ModelCommandHandler;

#[async_trait]
impl CommandHandler for ModelCommandHandler {
    fn name(&self) -> &str {
        "/model"
    }

    fn description(&self) -> &str {
        "Switch AI model for this thread"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let jyc_dir = context.thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await?;

        let override_path = jyc_dir.join("model-override");

        if context.args.is_empty() {
            // /model with no args — show current model and list available
            let current = if override_path.exists() {
                let model = tokio::fs::read_to_string(&override_path)
                    .await
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                format!("{model} (override)")
            } else {
                "default from config".to_string()
            };

            let models_list = if let Some(agent) = context.agent {
                match agent.base_url().await {
                    Ok(base_url) => {
                        let client = OpenCodeClient::new(&base_url);
                        match client.get_providers(&context.thread_path).await {
                            Ok(providers) => {
                                let mut all_models: Vec<String> = Vec::new();
                                for provider in &providers.all {
                                    for (_model_id, model) in &provider.models {
                                        all_models.push(format!("  - {}/{}", provider.id, model.id));
                                    }
                                }
                                all_models.sort();
                                if all_models.is_empty() {
                                    "\nNo models available from OpenCode server.".to_string()
                                } else {
                                    format!("\nAvailable models:\n{}", all_models.join("\n"))
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to fetch providers");
                                "\nFailed to fetch available models from OpenCode server.".to_string()
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to get OpenCode server URL");
                        "\nFailed to connect to OpenCode server.".to_string()
                    }
                }
            } else {
                "\nNo agent service available to list models.".to_string()
            };

            return Ok(CommandResult {
                success: true,
                message: format!(
                    "/model: current model is {current}.{}\n\nUse /model <model-id> to switch, /model reset to revert.",
                    models_list
                ),
                error: None,
                requires_restart: false,
            });
        }

        let arg = context.args.join(" ");

        if arg.to_lowercase() == "reset" {
            // /model reset — remove override, revert to config default
            if override_path.exists() {
                tokio::fs::remove_file(&override_path).await?;
            }
            // Model is passed per-prompt (PromptRequest.model), not per-session.
            // Session is preserved — AI keeps conversation memory.

            return Ok(CommandResult {
                success: true,
                message: "/model: reset to default model from config".into(),
                error: None,
                requires_restart: false,
            });
        }

        // /model <model-id> — write override
        tokio::fs::write(&override_path, arg.trim()).await?;

        // Model is passed per-prompt (PromptRequest.model), not per-session.
        // Session is preserved — AI keeps conversation memory.

        Ok(CommandResult {
            success: true,
            message: format!("/model: switched to {}", arg.trim()),
            error: None,
            requires_restart: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_context(thread_path: &Path) -> CommandContext {
        CommandContext {
            args: vec![],
            thread_path: thread_path.to_path_buf(),
            config: Arc::new(
                crate::config::load_config_from_str(
                    r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "opencode"
"#,
                )
                .unwrap(),
            ),
            channel: "test".into(),
            agent: None,
        }
    }

    #[tokio::test]
    async fn test_model_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ModelCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["SomeProvider/SomeModel".into()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!result.requires_restart); // model is per-prompt, no restart needed

        let override_content =
            tokio::fs::read_to_string(tmp.path().join(".jyc/model-override"))
                .await
                .unwrap();
        assert_eq!(override_content, "SomeProvider/SomeModel");
    }

    #[tokio::test]
    async fn test_model_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("model-override"), "old-model")
            .await
            .unwrap();

        let handler = ModelCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["reset".into()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!jyc_dir.join("model-override").exists());
    }

    #[tokio::test]
    async fn test_model_list() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ModelCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("current model is"));
        assert!(result.message.contains("No agent service available"));
    }
}
