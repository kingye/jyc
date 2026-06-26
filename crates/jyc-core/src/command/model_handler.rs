use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /model command — switch AI model for this thread.
///
/// Usage:
///   /model              List all available models
///   /model <provider/model-id>  Switch to the specified model
///   /model reset        Reset to default model from config
pub struct ModelCommandHandler;

#[async_trait]
impl CommandHandler for ModelCommandHandler {
    fn name(&self) -> &str {
        "/model"
    }

    fn description(&self) -> &str {
        "Switch AI model or list available models"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let jyc_dir = context.thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await?;

        let providers = &context.config.agent.providers;

        // Read current mode to determine which override file to use.
        let current_mode = crate::session_state::read_mode_override(&context.thread_path).await;

        if context.args.is_empty() {
            // /model — list all available models
            if providers.is_empty() {
                return Ok(CommandResult {
                    success: true,
                    message: "/model: no models configured".into(),
                    error: None,
                });
            }

            let mut lines = vec!["Available models:".to_string()];
            for (provider_name, provider_def) in providers {
                if provider_def.models.is_empty() {
                    lines.push(format!("  {provider_name}/*  (no specific models listed)"));
                } else {
                    for model_id in provider_def.models.keys() {
                        lines.push(format!("  {provider_name}/{model_id}"));
                    }
                }
            }

            Ok(CommandResult {
                success: true,
                message: lines.join("\n"),
                error: None,
            })
        } else if context.args.len() == 1 && context.args[0] == "reset" {
            // /model reset — remove mode-specific and legacy overrides
            let plan_path = jyc_dir.join("plan-model-override");
            let build_path = jyc_dir.join("build-model-override");
            let legacy_path = jyc_dir.join("model-override");
            let mut removed = false;
            for path in [&plan_path, &build_path, &legacy_path] {
                if path.exists() {
                    tokio::fs::remove_file(path).await?;
                    removed = true;
                }
            }
            if removed {
                Ok(CommandResult {
                    success: true,
                    message: "/model: reset to default model".into(),
                    error: None,
                })
            } else {
                Ok(CommandResult {
                    success: true,
                    message: "/model: already using default model".into(),
                    error: None,
                })
            }
        } else {
            // /model <provider/model-id> — switch model
            let model_id = context.args[0].clone();

            // Validate format: must be "provider/model-id"
            match model_id.split_once('/') {
                Some((provider_name, model_name))
                    if !provider_name.is_empty() && !model_name.is_empty() =>
                {
                    // Validate provider exists
                    let Some(provider_def) = providers.get(provider_name) else {
                        let available: Vec<&str> = providers.keys().map(|s| s.as_str()).collect();
                        return Ok(CommandResult {
                            success: false,
                            message: format!("/model: unknown provider '{provider_name}'"),
                            error: Some(format!(
                                "Provider '{provider_name}' not found. Available: {available:?}"
                            )),
                        });
                    };

                    // Validate model exists (if provider has specific models)
                    if !provider_def.models.is_empty()
                        && !provider_def.models.contains_key(model_name)
                    {
                        let available: Vec<&str> =
                            provider_def.models.keys().map(|s| s.as_str()).collect();
                        return Ok(CommandResult {
                            success: false,
                            message: format!(
                                "/model: unknown model '{model_name}' for provider '{provider_name}'"
                            ),
                            error: Some(format!(
                                "Model '{model_name}' not found in provider '{provider_name}'. Available: {available:?}"
                            )),
                        });
                    }

                    // Write mode-specific override file
                    let filename = match current_mode.as_deref() {
                        Some("plan") => "plan-model-override",
                        Some("build") => "build-model-override",
                        _ => "model-override",
                    };
                    let override_path = jyc_dir.join(filename);
                    tokio::fs::write(&override_path, &model_id).await?;

                    Ok(CommandResult {
                        success: true,
                        message: format!("/model: switched to {model_id}"),
                        error: None,
                    })
                }
                _ => Ok(CommandResult {
                    success: false,
                    message: format!("/model: invalid format '{model_id}'"),
                    error: Some(
                        "Expected 'provider/model-id' (e.g., 'anthropic/claude-opus-4-6')".into(),
                    ),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn test_context(thread_path: &Path) -> CommandContext {
        CommandContext {
            args: vec![],
            thread_path: thread_path.to_path_buf(),
            config: Arc::new(
                jyc_types::load_config_from_str(
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
mode = "agent"

[agent.providers.deepseek]
type = "openai-compatible"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[agent.providers.deepseek.models.deepseek-chat]
context_window = 64000

[agent.providers.deepseek.models.deepseek-reasoner]
context_window = 64000

[agent.providers.anthropic]
type = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"

[agent.providers.anthropic.models.claude-opus-4-6]
context_window = 200000
"#,
                )
                .unwrap(),
            ),
            channel: "test".into(),
            agent: None,
            template_dir: PathBuf::from("/tmp/test/templates"),
        }
    }

    #[tokio::test]
    async fn test_list_models() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_context(tmp.path());
        ctx.args = vec![];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("Available models:"));
        assert!(result.message.contains("deepseek/deepseek-chat"));
        assert!(result.message.contains("deepseek/deepseek-reasoner"));
        assert!(result.message.contains("anthropic/claude-opus-4-6"));
    }

    #[tokio::test]
    async fn test_list_models_empty_providers() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = CommandContext {
            args: vec![],
            thread_path: tmp.path().to_path_buf(),
            config: Arc::new(
                jyc_types::load_config_from_str(
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
mode = "agent"
"#,
                )
                .unwrap(),
            ),
            channel: "test".into(),
            agent: None,
            template_dir: PathBuf::from("/tmp/test/templates"),
        };
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("no models configured"));
    }

    #[tokio::test]
    async fn test_switch_model() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["deepseek/deepseek-chat".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result
                .message
                .contains("switched to deepseek/deepseek-chat")
        );

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/model-override"))
            .await
            .unwrap();
        assert_eq!(content, "deepseek/deepseek-chat");
    }

    #[tokio::test]
    async fn test_reset_model() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("model-override"), "deepseek/deepseek-chat\n")
            .await
            .unwrap();

        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["reset".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("reset to default model"));
        assert!(!jyc_dir.join("model-override").exists());
    }

    #[tokio::test]
    async fn test_reset_model_no_override() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["reset".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("already using default"));
    }

    #[tokio::test]
    async fn test_invalid_model_format() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["invalid-format".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("invalid format"));
    }

    #[tokio::test]
    async fn test_unknown_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["openai/gpt-5".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("unknown provider"));
    }

    #[tokio::test]
    async fn test_unknown_model() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["deepseek/non-existent-model".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("unknown model"));
    }

    #[tokio::test]
    async fn test_switch_model_in_plan_mode_writes_plan_override() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        // Simulate plan mode
        tokio::fs::write(jyc_dir.join("mode-override"), "plan\n")
            .await
            .unwrap();

        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["deepseek/deepseek-reasoner".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);

        // Should write to plan-model-override, not model-override
        assert!(jyc_dir.join("plan-model-override").exists());
        assert!(!jyc_dir.join("model-override").exists());
        let content = tokio::fs::read_to_string(jyc_dir.join("plan-model-override"))
            .await
            .unwrap();
        assert_eq!(content, "deepseek/deepseek-reasoner");
    }

    #[tokio::test]
    async fn test_switch_model_in_build_mode_writes_build_override() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        // Simulate build mode
        tokio::fs::write(jyc_dir.join("mode-override"), "build\n")
            .await
            .unwrap();

        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["deepseek/deepseek-chat".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);

        // Should write to build-model-override
        assert!(jyc_dir.join("build-model-override").exists());
        assert!(!jyc_dir.join("model-override").exists());
        let content = tokio::fs::read_to_string(jyc_dir.join("build-model-override"))
            .await
            .unwrap();
        assert_eq!(content, "deepseek/deepseek-chat");
    }

    #[tokio::test]
    async fn test_reset_clears_all_mode_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("plan-model-override"), "deepseek/some\n")
            .await
            .unwrap();
        tokio::fs::write(jyc_dir.join("build-model-override"), "ark/glm\n")
            .await
            .unwrap();
        tokio::fs::write(jyc_dir.join("model-override"), "legacy-model\n")
            .await
            .unwrap();

        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["reset".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("reset to default model"));

        assert!(!jyc_dir.join("plan-model-override").exists());
        assert!(!jyc_dir.join("build-model-override").exists());
        assert!(!jyc_dir.join("model-override").exists());
    }
}
