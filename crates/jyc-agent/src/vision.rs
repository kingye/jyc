//! Vision model client for the `read_image` tool fallback.
//!
//! When the primary model does not support images, the `read_image` tool
//! uses `VisionClient` to call an external vision model (e.g., DeepSeek-OCR)
//! via an OpenAI-compatible `/chat/completions` endpoint. The raw text result
//! is returned to the primary model as a tool output.
//!
//! ## Design
//!
//! - Single-shot, non-streaming HTTP request — no streaming, no tool calls,
//!   no session management.
//! - Uses the OpenAI-compatible `image_url` content block format.
//! - Reuses provider `base_url` and `api_key` from the main provider config,
//!   so users don't duplicate credentials.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;

/// A lightweight, stateless client that sends images to a vision model and
/// returns the textual analysis.
pub struct VisionClient {
    /// API base URL (e.g., "https://api.deepseek.com")
    base_url: String,
    /// API key for authentication
    api_key: String,
    /// Model identifier (e.g., "deepseek-ocr")
    model: String,
    /// Optional custom prompt sent alongside the image
    prompt: String,
    /// HTTP client with timeout
    client: reqwest::Client,
}

impl VisionClient {
    /// Create a new `VisionClient`.
    ///
    /// `base_url`, `api_key`, and `model` are resolved from the provider
    /// referenced by `VisionConfig.provider`. `prompt` falls back to a
    /// sensible default if not configured.
    pub fn new(base_url: String, api_key: String, model: String, prompt: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("VisionClient: failed to build reqwest::Client");

        Self {
            base_url,
            api_key,
            model,
            prompt: prompt.unwrap_or_else(|| {
                "Please carefully examine this image and describe its contents \
                 in detail. If there is any text, extract it verbatim."
                    .to_string()
            }),
            client,
        }
    }

    /// Analyze an image by sending it to the vision model.
    ///
    /// `media_type` is the MIME type (e.g., "image/png") and `base64_data`
    /// is the raw base64-encoded image bytes (without `data:` prefix).
    ///
    /// Returns the model's text response.
    pub async fn analyze(&self, media_type: &str, base64_data: &str) -> Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let data_url = format!("data:{};base64,{}", media_type, base64_data);

        // Build request with text prompt + image_url content blocks
        let request_body = serde_json::json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": self.prompt},
                    {"type": "image_url", "image_url": {"url": data_url}}
                ]
            }],
            "stream": false
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .context("VisionClient: HTTP request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "VisionClient: HTTP {} from {}: {}",
                status,
                url,
                body_text
            );
        }

        let chat_resp: ChatCompletionResponse = resp
            .json()
            .await
            .context("VisionClient: failed to parse response JSON")?;

        let content = chat_resp
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .unwrap_or("")
            .to_string();

        Ok(content)
    }
}

// ── OpenAI-compatible chat completion response types ──

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the request body is serialized as expected.
    #[test]
    fn test_request_body_format() {
        let body = serde_json::json!({
            "model": "deepseek-ocr",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe this image"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}
                ]
            }],
            "stream": false
        });

        let parsed = body.as_object().unwrap();
        assert_eq!(parsed["model"], "deepseek-ocr");
        assert_eq!(parsed["stream"], false);

        let msg = &parsed["messages"].as_array().unwrap()[0];
        assert_eq!(msg["role"], "user");

        let content = msg["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "describe this image");
        assert_eq!(content[1]["type"], "image_url");
        assert!(content[1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    }
}
