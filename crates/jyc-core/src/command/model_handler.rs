use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

use jyc_types::{ModelInfo, ProviderDef};

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::session_state;

/// /model command — switch AI model for this topic.
///
/// Usage:
///   /model              List all available models
///   /model <provider/model-id>  Switch to the specified model
///   /model reset        Reset to default model from config
pub struct ModelCommandHandler;

/// Returns the list of available models, same format as `/model` (no args).
///
/// This is the shared function used by both the command handler and the
/// inspect server, ensuring the dashboard popup shows the exact same list.
pub fn list_available_models(providers: &HashMap<String, ProviderDef>) -> Vec<ModelInfo> {
    let mut models = Vec::new();
    for (provider_name, provider_def) in providers {
        if provider_def.models.is_empty() {
            models.push(ModelInfo {
                name: format!("{provider_name}/*"),
            });
        } else {
            for model_id in provider_def.models.keys() {
                models.push(ModelInfo {
                    name: format!("{provider_name}/{model_id}"),
                });
            }
        }
    }
    models
}

#[async_trait]
impl CommandHandler for ModelCommandHandler {
    fn name(&self) -> &str {
        "/model"
    }

    fn description(&self) -> &str {
        "Switch AI model or list available models"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let jyc_dir = context.topic_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await?;

        let providers = &context.config.ai.providers;

        // Read current mode to determine which override file to use.
        // Chain: .jyc/mode-override > pattern mode from config > build default,
        // matching the agent runtime so the written override file actually
        // takes effect for pattern-mode topics.
        let current_mode = crate::session_state::resolve_effective_mode(
            &context.topic_path,
            &context.config,
            &context.channel,
        )
        .await;

        if context.args.is_empty() {
            // /model — list all available models
            let models = list_available_models(providers);
            if models.is_empty() {
                return Ok(CommandResult {
                    success: true,
                    message: "/model: no models configured".into(),
                    error: None,
                    append_body: None,
                });
            }

            let mut lines = vec!["Available models:".to_string()];
            for model in &models {
                lines.push(format!("  {}", model.name));
            }

            Ok(CommandResult {
                success: true,
                message: lines.join("\n"),
                error: None,
                append_body: None,
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
                    append_body: None,
                })
            } else {
                Ok(CommandResult {
                    success: true,
                    message: "/model: already using default model".into(),
                    error: None,
                    append_body: None,
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
                            append_body: None,
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
                            append_body: None,
                        });
                    }

                    // Write mode-specific override file
                    let filename = match current_mode.as_deref() {
                        Some("plan") => "plan-model-override",
                        Some("build") => "build-model-override",
                        _ => "build-model-override", // None = default build mode
                    };
                    let override_path = jyc_dir.join(filename);
                    tokio::fs::write(&override_path, &model_id).await?;

                    // Update `max_input_tokens` for the newly-active model.
                    if let Some(new_max) = session_state::resolve_active_context_window(
                        &context.topic_path,
                        &context.config,
                        &context.channel,
                        context.config.ai.auto_reset_threshold,
                    )
                    .await
                    {
                        session_state::write_max_input_tokens(&context.topic_path, new_max).await;
                    }

                    Ok(CommandResult {
                        success: true,
                        message: format!("/model: switched to {model_id}"),
                        error: None,
                        append_body: None,
                    })
                }
                _ => Ok(CommandResult {
                    success: false,
                    message: format!("/model: invalid format '{model_id}'"),
                    error: Some(
                        "Expected 'provider/model-id' (e.g., 'anthropic/claude-opus-4-6')".into(),
                    ),
                    append_body: None,
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

    fn test_context(topic_path: &Path) -> CommandContext {
        CommandContext {
            args: vec![],
            topic_path: topic_path.to_path_buf(),
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
            template_dirs: PathBuf::from("/tmp/test/templates").into(),
            channel_type: "websocket".to_string(),
            config_path: None,
            topic: "test".to_string(),
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
            topic_path: tmp.path().to_path_buf(),
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
            template_dirs: PathBuf::from("/tmp/test/templates").into(),
            channel_type: "websocket".to_string(),
            config_path: None,
            topic: "test".to_string(),
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

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/build-model-override"))
            .await
            .unwrap();
        assert_eq!(content, "deepseek/deepseek-chat");
    }

    #[tokio::test]
    async fn test_reset_model() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("build-model-override"),
            "deepseek/deepseek-chat\n",
        )
        .await
        .unwrap();

        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["reset".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("reset to default model"));
        assert!(!jyc_dir.join("build-model-override").exists());
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

    /// #615: topic mode comes from pattern config (`.jyc/pattern` set, no
    /// `mode-override` file) — `/model` must still write the plan-specific
    /// override file so the runtime picks it up.
    #[tokio::test]
    async fn test_switch_model_with_pattern_mode_writes_plan_override() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        // Topic created by pattern "p1" (mode = "plan" in config below);
        // no .jyc/mode-override file.
        tokio::fs::write(jyc_dir.join("pattern"), "p1\n")
            .await
            .unwrap();

        let mut ctx = test_context(tmp.path());
        ctx.config = Arc::new(
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
[[channels.test.patterns]]
name = "p1"
mode = "plan"
[agent]
enabled = true
mode = "agent"

[agent.providers.deepseek]
type = "openai-compatible"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[agent.providers.deepseek.models.deepseek-reasoner]
context_window = 64000
"#,
            )
            .unwrap(),
        );
        ctx.args = vec!["deepseek/deepseek-reasoner".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);

        // Pattern mode "plan" must select the plan override file.
        assert!(jyc_dir.join("plan-model-override").exists());
        assert!(!jyc_dir.join("build-model-override").exists());
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

    #[tokio::test]
    async fn test_model_writes_max_input_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        // Pre-existing session with a different max_input_tokens
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"max_input_tokens":100}"#,
        )
        .await
        .unwrap();

        let mut ctx = test_context(tmp.path());
        ctx.config = Arc::new(
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
auto_reset_threshold = 0.95

[agent.providers.deepseek]
type = "openai-compatible"
base_url = "https://x"
api_key_env = "X"

[agent.providers.deepseek.models.deepseek-chat]
context_window = 64000
"#,
            )
            .unwrap(),
        );
        ctx.args = vec!["deepseek/deepseek-chat".into()];
        let handler = ModelCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);

        // build-model-override was written
        assert!(jyc_dir.join("build-model-override").exists());
        // and max_input_tokens in agent-session.json reflects the new model
        let content = tokio::fs::read_to_string(jyc_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["max_input_tokens"], 60800); // 95% of 64000
    }
}
