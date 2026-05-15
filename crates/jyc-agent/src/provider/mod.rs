//! LLM provider abstraction.
//!
//! Defines the `Provider` trait and implementations for:
//! - Anthropic Messages API (native)
//! - OpenAI-compatible Chat Completions API

pub mod anthropic;
pub mod openai_compat;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

use crate::types::{Message, StreamEvent, ToolDefinition};

/// Stream of events from an LLM provider.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

/// Trait for LLM providers.
///
/// Minimal interface: send messages with tools, get a streaming response.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider name (e.g., "anthropic", "deepseek").
    fn name(&self) -> &str;

    /// Model identifier being used.
    fn model(&self) -> &str;

    /// Send messages and get a streaming response.
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
    ) -> Result<EventStream>;
}

/// Create a provider from configuration.
///
/// Parses the model string (format: "provider_name/model_id") and creates
/// the appropriate provider instance.
///
/// Supports formats:
/// - "anthropic/claude-opus-4-6" → provider="anthropic", model="claude-opus-4-6"
/// - "deepseek/deepseek-v4-pro" → provider="deepseek", model="deepseek-v4-pro"
/// - "ark/ep-xxxxx" → provider="ark", model="ep-xxxxx"
///
/// The provider_name must match a key in the `[agent.providers.*]` config.
pub fn create_provider(
    model: &str,
    providers: &std::collections::HashMap<String, crate::types::ProviderConfig>,
) -> Result<Box<dyn Provider>> {
    let (provider_name, model_id) = model
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!(
            "Invalid model format '{}'. Expected 'provider/model-id' (e.g., 'anthropic/claude-opus-4-6')",
            model
        ))?;

    let config = providers
        .get(provider_name)
        .ok_or_else(|| anyhow::anyhow!(
            "Provider '{}' not found in [agent.providers]. Available: {:?}. \
             Add [agent.providers.{}] to config.toml.",
            provider_name,
            providers.keys().collect::<Vec<_>>(),
            provider_name
        ))?;

    // Read API key from environment
    let api_key = if let Some(env_var) = &config.api_key_env {
        std::env::var(env_var).ok()
    } else {
        None
    };

    // Resolve params: model-level overrides provider-level (shallow merge)
    let params = resolve_params(config.params.as_ref(), config.models.get(model_id).and_then(|m| m.params.as_ref()));

    match config.provider_type.as_str() {
        "anthropic" => {
            let base_url = config.base_url.as_deref()
                .unwrap_or("https://api.anthropic.com/v1");
            Ok(Box::new(anthropic::AnthropicProvider::new(
                base_url,
                model_id,
                api_key.as_deref(),
                params,
            )?))
        }
        "openai-compatible" | "openai" => {
            let base_url = config.base_url.as_deref()
                .ok_or_else(|| anyhow::anyhow!("OpenAI-compatible provider '{}' requires base_url", provider_name))?;
            Ok(Box::new(openai_compat::OpenAiCompatProvider::new(
                base_url,
                model_id,
                api_key.as_deref(),
                params,
            )?))
        }
        other => anyhow::bail!("Unknown provider type '{}' for provider '{}'", other, provider_name),
    }
}

/// Merge provider-level params with model-level params.
/// Model params override provider params (shallow merge of top-level keys).
fn resolve_params(
    provider_params: Option<&serde_json::Value>,
    model_params: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    match (provider_params, model_params) {
        (None, None) => None,
        (Some(p), None) => Some(p.clone()),
        (None, Some(m)) => Some(m.clone()),
        (Some(p), Some(m)) => {
            // Shallow merge: model keys override provider keys
            let mut merged = p.clone();
            if let (Some(base), Some(overlay)) = (merged.as_object_mut(), m.as_object()) {
                for (k, v) in overlay {
                    base.insert(k.clone(), v.clone());
                }
            }
            Some(merged)
        }
    }
}
