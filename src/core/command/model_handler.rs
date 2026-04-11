use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use std::path::{Path, PathBuf};

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::services::opencode::client::OpenCodeClient;

/// Clean pattern by removing unnecessary escapes that might come from email systems.
/// If pattern is "ark\*" (likely from email escaping), convert to "ark*".
fn clean_pattern(pattern: &str) -> String {
    let mut result = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Check if next character is * or ?
            if let Some(&next_c) = chars.peek() {
                if next_c == '*' || next_c == '?' {
                    // Skip the backslash, keep the * or ? as wildcard
                    chars.next(); // consume the wildcard
                    result.push(next_c);
                    continue;
                }
            }
            // Keep the backslash for other cases
            result.push(c);
        } else {
            result.push(c);
        }
    }
    
    result
}

/// Convert wildcard pattern (* and ?) to case-insensitive regex.
/// Pattern is anchored to start of string.
/// If pattern contains no wildcards (* or ?), add .* at the end for convenience.
/// Handles escaped characters (e.g., "\*" becomes literal "*", not wildcard).
fn wildcard_to_regex(pattern: &str) -> String {
    let mut regex = String::with_capacity(pattern.len() * 2);
    regex.push('^'); // Anchor to start
    
    let mut has_wildcard = false;
    let mut chars = pattern.chars().peekable();
    
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Handle escape sequence
            if let Some(next_c) = chars.next() {
                // Escaped character, add as literal
                match next_c {
                    // These need escaping in regex even when escaped in pattern
                    '.' | '+' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
                        regex.push('\\');
                        regex.push(next_c);
                    }
                    // * and ? when escaped become literal characters
                    '*' | '?' => {
                        regex.push(next_c);
                    }
                    _ => {
                        regex.push(next_c);
                    }
                }
            } else {
                // Trailing backslash, add as literal
                regex.push('\\');
            }
        } else {
            match c {
                '*' => {
                    regex.push_str(".*");
                    has_wildcard = true;
                }
                '?' => {
                    regex.push('.');
                    has_wildcard = true;
                }
                // Regex special characters that need escaping
                '.' | '+' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' => {
                    regex.push('\\');
                    regex.push(c);
                }
                _ => regex.push(c),
            }
        }
    }
    
    // Add .* at the end only if pattern has no wildcards
    // This makes simple patterns like "ark" match "ark/deepseek"
    // while patterns with wildcards match exactly as specified
    if !has_wildcard {
        regex.push_str(".*");
    }
    
    format!("(?i){}", regex) // Case-insensitive flag
}

/// Fetch and filter models from providers.
/// If pattern is Some, apply wildcard filtering.
async fn filter_models(
    agent: Option<std::sync::Arc<dyn crate::services::agent::AgentService>>,
    thread_path: &Path,
    pattern: Option<&str>,
) -> Result<Vec<String>> {
    // Fetch providers from OpenCode
    let providers = if let Some(agent) = agent {
        match agent.base_url().await {
            Ok(base_url) => {
                let client = OpenCodeClient::new(&base_url);
                match client.get_providers(thread_path).await {
                    Ok(providers) => providers,
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to fetch providers");
                        return Err(e.context("Failed to fetch providers from OpenCode server"));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to get OpenCode server URL");
                return Err(e.context("Failed to connect to OpenCode server"));
            }
        }
    } else {
        return Err(anyhow::anyhow!("No agent service available to list models"));
    };

    // Collect all models
    let mut all_models: Vec<String> = Vec::new();
    for provider in &providers.all {
        for (_model_id, model) in &provider.models {
            all_models.push(format!("{}/{}", provider.id, model.id));
        }
    }

    // Apply filtering if pattern exists
    if let Some(pattern) = pattern {
        if pattern.is_empty() {
            // Empty pattern means no filtering
        } else {
            // Clean pattern to handle email escaping (e.g., "ark\*" -> "ark*")
            let cleaned_pattern = clean_pattern(pattern);
            let regex_pattern = wildcard_to_regex(&cleaned_pattern);
            let re = Regex::new(&regex_pattern)
                .with_context(|| format!("Invalid pattern: '{}' (use * and ? wildcards)", pattern))?;
            
            all_models.retain(|model| re.is_match(model));
        }
    }

    all_models.sort();
    Ok(all_models)
}

/// /model command — switch AI model for this thread.
///
/// Usage:
///   /model ls [pattern]  List models (use * and ? wildcards, case-insensitive)
///   /model reset         Reset to default model from config
///   /model <model-id>    Switch to a specific model
/// 
/// Examples:
///   /model ls            List all models
///   /model ls ark        List models from ark provider
///   /model ls *seek*     List models containing "seek"
///   /model ls ark?deep   List models like "arkXdeep" (single char wildcard)
pub struct ModelCommandHandler;

#[async_trait]
impl CommandHandler for ModelCommandHandler {
    fn name(&self) -> &str {
        "/model"
    }

    fn description(&self) -> &str {
        "Switch AI model for this thread, list models with wildcard filtering"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let jyc_dir = context.thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await?;

        let override_path = jyc_dir.join("model-override");

        // Check if no arguments provided
        if context.args.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: "/model requires arguments: ls [pattern], reset, or <model-id>".into(),
                error: Some("No arguments provided".into()),
                requires_restart: false,
            });
        }

        // Dispatch based on first argument
        match context.args.first().map(|s| s.as_str()) {
            Some("ls") => {
                // Handle /model ls [pattern]
                let pattern = context.args.get(1).map(|s| s.as_str());
                
                // Get current model info
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

                // Fetch and filter models
                match filter_models(context.agent.clone(), &context.thread_path, pattern).await {
                    Ok(filtered_models) => {
                        if filtered_models.is_empty() {
                            let no_match_msg = if let Some(pattern) = pattern {
                                if pattern.is_empty() {
                                    "\nNo models available from OpenCode server.".to_string()
                                } else {
                                    format!("\nNo models found matching pattern: '{}'", pattern)
                                }
                            } else {
                                "\nNo models available from OpenCode server.".to_string()
                            };
                            
                            return Ok(CommandResult {
                                success: true,
                                message: format!("/model: current model is {current}.{}", no_match_msg),
                                error: None,
                                requires_restart: false,
                            });
                        } else {
                            let formatted_models: Vec<String> = filtered_models
                                .iter()
                                .map(|model| format!("  - {}", model))
                                .collect();
                            
                            return Ok(CommandResult {
                                success: true,
                                message: format!(
                                    "/model: current model is {current}.\n\nAvailable models:\n{}",
                                    formatted_models.join("\n")
                                ),
                                error: None,
                                requires_restart: false,
                            });
                        }
                    }
                    Err(e) => {
                        return Ok(CommandResult {
                            success: false,
                            message: format!("/model: failed to list models: {}", e),
                            error: Some(e.to_string()),
                            requires_restart: false,
                        });
                    }
                }
            }
            Some("reset") => {
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
            Some(_) => {
                // Assume it's a model ID to switch to
                let arg = context.args.join(" ");
                
                // /model <model-id> — write override
                tokio::fs::write(&override_path, arg.trim()).await?;

                // Model is passed per-prompt (PromptRequest.model), not per-session.
                // Session is preserved — AI keeps conversation memory.

                return Ok(CommandResult {
                    success: true,
                    message: format!("/model: switched to {}", arg.trim()),
                    error: None,
                    requires_restart: false,
                });
            }
            None => {
                // Should not happen due to earlier check, but handle anyway
                return Ok(CommandResult {
                    success: false,
                    message: "/model requires arguments: ls [pattern], reset, or <model-id>".into(),
                    error: Some("No arguments provided".into()),
                    requires_restart: false,
                });
            }
        }
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
            template_dir: PathBuf::from("/tmp/test/templates"),
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
    async fn test_model_no_args_error() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ModelCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success); // Should fail with no args
        assert!(result.message.contains("requires arguments"));
    }

    #[test]
    fn test_wildcard_to_regex() {
        // Test simple prefix (no wildcards, gets .* at end)
        assert_eq!(wildcard_to_regex("ark"), "(?i)^ark.*");
        
        // Test wildcard * (has wildcard, no trailing .*)
        assert_eq!(wildcard_to_regex("ark*"), "(?i)^ark.*");
        assert_eq!(wildcard_to_regex("*seek*"), "(?i)^.*seek.*");
        
        // Test single char wildcard ? (has wildcard, no trailing .*)
        assert_eq!(wildcard_to_regex("ark?"), "(?i)^ark.");
        assert_eq!(wildcard_to_regex("ark?deep"), "(?i)^ark.deep");
        
        // Test special characters escaping (no wildcards, gets .* at end)
        assert_eq!(wildcard_to_regex("test.com"), "(?i)^test\\.com.*");
        assert_eq!(wildcard_to_regex("a+b"), "(?i)^a\\+b.*");
        
        // Test multiple wildcards (has wildcard, no trailing .*)
        assert_eq!(wildcard_to_regex("ark*model*v3"), "(?i)^ark.*model.*v3");
    }

    #[test]
    fn test_wildcard_matching() {
        // Test simple prefix matching
        let re = Regex::new(&wildcard_to_regex("ark")).unwrap();
        assert!(re.is_match("ark/deepseek-v3.2"));
        assert!(re.is_match("ARK/deepseek-coder")); // case-insensitive
        assert!(!re.is_match("anthropic/claude"));
        
        // Test * wildcard
        let re = Regex::new(&wildcard_to_regex("*seek*")).unwrap();
        assert!(re.is_match("ark/deepseek-v3.2"));
        assert!(re.is_match("openai/gpt-4-seeker"));
        assert!(!re.is_match("anthropic/claude"));
        
        // Test ? wildcard
        let re = Regex::new(&wildcard_to_regex("ark?deep")).unwrap();
        assert!(re.is_match("arkXdeepseek-v3"));
        assert!(re.is_match("ark-deepseek-coder"));
        assert!(!re.is_match("arkdeepseek")); // missing char
        assert!(!re.is_match("arkXXdeepseek")); // too many chars
        
        // Test mixed wildcards
        let re = Regex::new(&wildcard_to_regex("ark*model*v3")).unwrap();
        assert!(re.is_match("ark/deepseek-model-v3.2"));
        assert!(re.is_match("ark/some-model-v3"));
        assert!(!re.is_match("ark/model-v2"));
        
        // Test that ark and ark* behave the same (both match ark models)
        let re_ark = Regex::new(&wildcard_to_regex("ark")).unwrap();
        let re_ark_star = Regex::new(&wildcard_to_regex("ark*")).unwrap();
        
        let test_models = vec![
            "ark/deepseek-v3.2",
            "ark/deepseek-coder-v2",
            "ark123/some-model",
            "anthropic/claude-3.5-sonnet",
        ];
        
        for model in test_models {
            let ark_match = re_ark.is_match(model);
            let ark_star_match = re_ark_star.is_match(model);
            assert_eq!(ark_match, ark_star_match, 
                       "ark and ark* should match the same for model: {}", model);
        }
    }
}
