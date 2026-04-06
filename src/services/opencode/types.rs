use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// --- API Request/Response Types ---

/// Body for session creation.
#[derive(Debug, Serialize)]
pub struct CreateSessionRequest {
    pub title: String,
}

/// Model reference for prompt requests (provider + model ID).
#[derive(Debug, Clone, Serialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

impl ModelRef {
    /// Parse a combined model string like "provider/model-id" into a ModelRef.
    ///
    /// Returns None if the string doesn't contain a "/" separator.
    pub fn from_combined(combined: &str) -> Option<Self> {
        let (provider, model) = combined.split_once('/')?;
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            provider_id: provider.to_string(),
            model_id: model.to_string(),
        })
    }
}

/// Body for prompt requests (both async and blocking).
#[derive(Debug, Serialize)]
pub struct PromptRequest {
    pub system: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub parts: Vec<PromptPart>,
}

/// A single part in a prompt request.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum PromptPart {
    #[serde(rename = "text")]
    Text { text: String },
}

/// Session info returned by the API.
#[derive(Debug, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// Response from blocking prompt.
#[derive(Debug, Deserialize)]
pub struct PromptResponse {
    pub data: Option<PromptResponseData>,
}

#[derive(Debug, Deserialize)]
pub struct PromptResponseData {
    #[serde(default)]
    pub parts: Vec<ResponsePart>,
    #[serde(default)]
    pub info: Option<ResponseInfo>,
}

#[derive(Debug, Deserialize)]
pub struct ResponseInfo {
    pub error: Option<ResponseError>,
}

#[derive(Debug, Deserialize)]
pub struct ResponseError {
    pub name: String,
    #[serde(default)]
    pub data: Option<HashMap<String, serde_json::Value>>,
}

// --- SSE Event Types ---

/// A parsed SSE event from the OpenCode event stream.
#[derive(Debug, Deserialize)]
pub struct SseEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub properties: serde_json::Value,
}

/// A message part from SSE `message.part.updated` events.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponsePart {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(rename = "type")]
    pub part_type: String,
    // Text part
    #[serde(default)]
    pub text: Option<String>,
    // Tool part
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub state: Option<ToolState>,
    // Step-finish part
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub tokens: Option<serde_json::Value>,
}

/// State of a tool invocation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolState {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Message info from `message.updated` events.
#[derive(Debug, Deserialize)]
pub struct MessageInfo {
    #[serde(default, rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default, rename = "providerID")]
    pub provider_id: Option<String>,
    #[serde(default, rename = "modelID")]
    pub model_id: Option<String>,
    /// The agent mode OpenCode actually used (e.g., "build", "plan").
    #[serde(default)]
    pub mode: Option<String>,
}

/// Session status from `session.status` events.
#[derive(Debug, Deserialize)]
pub struct SessionStatus {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub status: SessionStatusDetail,
}

#[derive(Debug, Deserialize)]
pub struct SessionStatusDetail {
    #[serde(rename = "type")]
    pub status_type: String,
    #[serde(default)]
    pub attempt: Option<u32>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Session error from `session.error` events.
#[derive(Debug, Deserialize)]
pub struct SessionError {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub error: SessionErrorDetail,
}

#[derive(Debug, Deserialize)]
pub struct SessionErrorDetail {
    pub name: String,
    #[serde(default)]
    pub data: Option<HashMap<String, serde_json::Value>>,
}

// --- Result types ---

/// Result of AI reply generation.
#[derive(Debug)]
pub struct GenerateReplyResult {
    /// Whether reply was sent by MCP tool (vs fallback)
    pub reply_sent_by_tool: bool,
    /// Accumulated text from AI (for fallback direct send)
    pub reply_text: Option<String>,
    /// Model used for generation
    pub model_id: Option<String>,
    /// Provider used
    pub provider_id: Option<String>,
    /// Mode used for generation
    pub mode: Option<String>,
}

/// Model information.
#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Provider information.
#[derive(Debug, Deserialize)]
pub struct Provider {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub models: HashMap<String, Model>,
}

/// Response from /provider endpoint.
#[derive(Debug, Deserialize)]
pub struct ProvidersResponse {
    #[serde(default)]
    pub all: Vec<Provider>,
    #[serde(default)]
    pub default: HashMap<String, String>,
    #[serde(default)]
    pub connected: Vec<String>,
}

/// Token usage information from step-finish events.
#[derive(Debug, Deserialize)]
pub struct TokenInfo {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cache: CacheTokenInfo,
}

/// Cache token usage information.
#[derive(Debug, Default, Deserialize)]
pub struct CacheTokenInfo {
    #[serde(default)]
    pub read: u64,
    #[serde(default)]
    pub write: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_info_parsing() {
        // Test parsing of complete token info
        let json_data = r#"
        {
            "input": 1500,
            "output": 250,
            "reasoning": 300,
            "cache": {
                "read": 100,
                "write": 50
            }
        }
        "#;

        let token_info: TokenInfo = serde_json::from_str(json_data).unwrap();
        assert_eq!(token_info.input, 1500);
        assert_eq!(token_info.output, 250);
        assert_eq!(token_info.reasoning, 300);
        assert_eq!(token_info.cache.read, 100);
        assert_eq!(token_info.cache.write, 50);
    }

    #[test]
    fn test_token_info_with_missing_fields() {
        // Test parsing with missing fields (should use defaults)
        let json_data = r#"
        {
            "input": 1000,
            "output": 200
        }
        "#;

        let token_info: TokenInfo = serde_json::from_str(json_data).unwrap();
        assert_eq!(token_info.input, 1000);
        assert_eq!(token_info.output, 200);
        assert_eq!(token_info.reasoning, 0); // default
        assert_eq!(token_info.cache.read, 0); // default
        assert_eq!(token_info.cache.write, 0); // default
    }

    #[test]
    fn test_cache_token_info_default() {
        let cache_info = CacheTokenInfo::default();
        assert_eq!(cache_info.read, 0);
        assert_eq!(cache_info.write, 0);
    }
}
