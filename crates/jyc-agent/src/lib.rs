//! In-process AI agent for the JYC framework.
//!
//! Replaces the external OpenCode server with a self-contained Rust agent
//! that runs LLM inference and tool execution in-process.

pub mod provider;
pub mod tools;
pub mod types;
pub mod agent_loop;
pub mod session;
pub mod service;

pub use service::JycAgentService;

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::provider::{self, Provider};
    use crate::types::{Message, ProviderConfig};
    use futures::StreamExt;
    use std::collections::HashMap;

    /// Manual integration test — requires local proxy running.
    /// Run with: cargo test -p jyc-agent -- --ignored test_anthropic_streaming
    #[tokio::test]
    #[ignore]
    async fn test_anthropic_streaming() {
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), ProviderConfig {
            provider_type: "anthropic".to_string(),
            base_url: Some("http://localhost:6655/anthropic/v1".to_string()),
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
        });

        let provider = provider::create_provider("anthropic/claude-opus-4-6", &providers).unwrap();
        let messages = vec![Message::user("Say hello in exactly 3 words.")];
        let stream = provider.complete(&messages, &[], "You are a helpful assistant.").await.unwrap();

        tokio::pin!(stream);
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                types::StreamEvent::TextDelta(t) => text.push_str(&t),
                types::StreamEvent::Done => break,
                _ => {}
            }
        }

        assert!(!text.is_empty(), "Expected non-empty response");
        println!("Response: {}", text);
    }

    /// Manual integration test for full agent loop.
    /// Run with: cargo test -p jyc-agent -- --ignored test_agent_loop_simple
    #[tokio::test]
    #[ignore]
    async fn test_agent_loop_simple() {
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), ProviderConfig {
            provider_type: "anthropic".to_string(),
            base_url: Some("http://localhost:6655/anthropic/v1".to_string()),
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
        });

        let provider = provider::create_provider("anthropic/claude-opus-4-6", &providers).unwrap();

        // Create a temp dir as working directory
        let tmp = tempfile::tempdir().unwrap();

        // Create tool registry with just bash
        let registry = tools::builtin::create_builtin_registry();

        let cancel = tokio_util::sync::CancellationToken::new();
        let result = agent_loop::run(
            provider.as_ref(),
            &registry,
            "You are a helpful assistant. Reply concisely.",
            "What is 2+2? Use the bash tool to compute it with `echo $((2+2))`",
            tmp.path(),
            cancel,
        ).await.unwrap();

        println!("Text: {}", result.text);
        println!("Input tokens: {}", result.input_tokens);
        println!("Output tokens: {}", result.output_tokens);
        assert!(result.text.contains("4"), "Expected result to contain '4'");
    }
}
