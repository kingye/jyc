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
            "Provider '{}' not found in config. Available: {:?}",
            provider_name,
            providers.keys().collect::<Vec<_>>()
        ))?;

    // Read API key from environment
    let api_key = if let Some(env_var) = &config.api_key_env {
        std::env::var(env_var).ok()
    } else {
        None
    };

    match config.provider_type.as_str() {
        "anthropic" => {
            let base_url = config.base_url.as_deref()
                .unwrap_or("https://api.anthropic.com/v1");
            Ok(Box::new(anthropic::AnthropicProvider::new(
                base_url,
                model_id,
                api_key.as_deref(),
            )?))
        }
        "openai-compatible" | "openai" => {
            let base_url = config.base_url.as_deref()
                .ok_or_else(|| anyhow::anyhow!("OpenAI-compatible provider '{}' requires base_url", provider_name))?;
            Ok(Box::new(openai_compat::OpenAiCompatProvider::new(
                base_url,
                model_id,
                api_key.as_deref(),
            )?))
        }
        other => anyhow::bail!("Unknown provider type '{}' for provider '{}'", other, provider_name),
    }
}
