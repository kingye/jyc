//! Webfetch tool — fetch web content.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// Maximum response size (512KB).
const MAX_RESPONSE_SIZE: usize = 512 * 1024;

/// Webfetch tool — fetches content from URLs.
pub struct WebfetchTool;

#[async_trait]
impl Tool for WebfetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }

    fn description(&self) -> &str {
        "Fetch content from a URL. Returns the response body as text. \
         Use for retrieving web pages, API responses, or downloading content."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 30)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let url = input.get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'url' parameter"))?;

        let timeout_secs = input.get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(30);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))?;

        let resp = client.get(url)
            .header("user-agent", "jyc-agent/0.1")
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Ok(ToolOutput::error(format!(
                "HTTP {} {}", status.as_u16(), status.canonical_reason().unwrap_or("Unknown")
            )));
        }

        let body = resp.text().await
            .map_err(|e| anyhow::anyhow!("Failed to read response: {e}"))?;

        let mut result = body;
        if result.len() > MAX_RESPONSE_SIZE {
            result.truncate(MAX_RESPONSE_SIZE);
            result.push_str("\n... [response truncated]");
        }

        Ok(ToolOutput::success(result))
    }
}
