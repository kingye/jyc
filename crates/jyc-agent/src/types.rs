//! Core types for the agent system.
//!
//! Inspired by jcode's clean architecture but minimal — only what JYC needs.

use serde::{Deserialize, Serialize};

/// Role in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// Source of image bytes in a content block.
///
/// Carries the canonical bytes-or-url + mime so each provider can choose its
/// own wire format. OpenAI-compatible servers map both `Base64` and `Url`
/// onto the same `image_url.url` field (using `data:` URLs for base64);
/// Anthropic uses distinct `source.type = "base64"` vs `"url"` shapes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    /// Inline base64-encoded image bytes (no `data:` prefix).
    Base64 { media_type: String, data: String },
    /// Remote http(s) URL — the provider fetches it.
    Url { url: String },
}

/// A content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text content.
    Text { text: String },
    /// Image content (multimodal input).
    Image { source: ImageSource },
    /// A tool use request from the assistant.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool result provided back to the assistant.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Create a user message with text.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Create a user message with arbitrary content blocks (text + images).
    pub fn user_with_blocks(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content: blocks,
        }
    }

    /// Create an assistant message with text.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Create a tool result message.
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
                is_error,
            }],
        }
    }

    /// Extract all text content from this message.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Extract all tool use blocks from this message.
    pub fn tool_uses(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
            .collect()
    }
}

/// Events streamed from the LLM provider.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of text from the assistant.
    TextDelta(String),
    /// A chunk of reasoning/thinking content (provider-specific, e.g., DeepSeek).
    ReasoningDelta(String),
    /// Start of a tool use block.
    ToolUseStart { id: String, name: String },
    /// A chunk of tool input JSON.
    ToolInputDelta(String),
    /// End of a tool use block (input is complete).
    ToolUseEnd,
    /// Token usage information.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Stream is complete.
    Done,
    /// An error occurred.
    Error(String),
}

/// JSON Schema definition for a tool (sent to the LLM).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Result of running the agent loop.
#[derive(Debug)]
pub struct AgentLoopResult {
    /// Final text response from the assistant.
    pub text: String,
    /// Whether the reply_message tool was called successfully.
    pub reply_sent_by_tool: bool,
    /// Reply text extracted from the reply_message tool call (if used).
    pub reply_text_from_tool: Option<String>,
    /// Total input tokens used across all turns.
    pub input_tokens: u64,
    /// Total output tokens used across all turns.
    pub output_tokens: u64,
    /// The full conversation history (internal format for logic).
    pub history: Vec<Message>,
    /// Raw provider-formatted context (for persistence in agent-context.json).
    /// This preserves provider-specific fields like DeepSeek's reasoning_content.
    pub raw_context: Vec<serde_json::Value>,
}

/// Provider configuration from config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider type: "anthropic" or "openai-compatible"
    #[serde(rename = "type")]
    pub provider_type: String,
    /// API base URL (optional, uses default for provider type)
    pub base_url: Option<String>,
    /// Environment variable name containing the API key
    pub api_key_env: Option<String>,
    /// Default context window size in tokens
    pub context_window: Option<u64>,
    /// Whether models under this provider can accept image content blocks
    /// (multimodal input). Per-model override via `ModelConfig.supports_images`
    /// takes precedence. Default: false.
    pub supports_images: Option<bool>,
    /// Extra parameters merged into every API request for this provider
    pub params: Option<serde_json::Value>,
    /// Optional User-Agent header override for all models under this provider.
    /// Model-level `user_agent` takes precedence.
    pub user_agent: Option<String>,
    /// Per-model configuration
    #[serde(default)]
    pub models: std::collections::HashMap<String, ModelConfig>,
}

/// Per-model configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Context window size in tokens for this specific model
    pub context_window: Option<u64>,
    /// Whether this specific model can accept image content blocks
    /// (multimodal input). Overrides the provider-level `supports_images`.
    /// Default: inherits from provider, else false.
    pub supports_images: Option<bool>,
    /// Extra parameters merged into API request (overrides provider params)
    pub params: Option<serde_json::Value>,
    /// Optional User-Agent header override for requests made by this model.
    /// Takes precedence over provider-level `user_agent`.
    pub user_agent: Option<String>,
}

/// Vision model configuration for the `read_image` tool fallback.
///
/// When the primary model does not support images (`supports_images = false`),
/// the `read_image` tool uses this configuration to call an independent vision
/// model (e.g., DeepSeek-OCR) to analyze images and return text descriptions.
///
/// The `provider` field references a named entry in `[agent.providers.xxx]`
/// to reuse its `base_url` and `api_key_env`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionConfig {
    /// Whether vision fallback is enabled (default: false)
    #[serde(default)]
    pub enabled: bool,
    /// Name of the provider in `[agent.providers]` to use for vision calls
    pub provider: String,
    /// Model identifier (e.g., "deepseek-ocr")
    pub model: String,
    /// Optional custom prompt for the vision model
    pub prompt: Option<String>,
}

/// Agent configuration section from config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Default model in "provider/model-id" format
    pub model: Option<String>,
    /// Optional small/fast model used for ancillary LLM work — currently:
    /// - cycle-boundary progress summary (in `agent_loop`)
    /// - between-messages context reset summary (in `session::summarize_context`)
    /// Falls back to the main `model` if unset, or if provider construction
    /// fails (logged as a warning, the agent loop continues).
    #[allow(clippy::doc_lazy_continuation)]
    #[serde(default)]
    pub small_model: Option<String>,
    /// Provider definitions
    #[serde(default)]
    pub providers: std::collections::HashMap<String, ProviderConfig>,
    /// Maximum agent loop iterations per cycle. Default: 500.
    /// When exceeded, agent sends progress reply, resets counter, and continues.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// Vision fallback configuration for text-only models to use an external
    /// vision model (e.g., DeepSeek-OCR) for image analysis via `read_image`.
    pub vision: Option<VisionConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: None,
            small_model: None,
            providers: std::collections::HashMap::new(),
            max_iterations: default_max_iterations(),
            vision: None,
        }
    }
}

fn default_max_iterations() -> usize {
    500
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn role_serde_roundtrip() {
        let role = Role::User;
        let json = serde_json::to_string(&role).unwrap();
        assert_eq!(json, "\"user\"");
        let deserialized: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(role, deserialized);
    }

    #[test]
    fn image_source_base64() {
        let img = ImageSource::Base64 {
            media_type: "image/png".to_string(),
            data: "abc123".to_string(),
        };
        let json = serde_json::to_string(&img).unwrap();
        assert!(json.contains("base64"));
        assert!(json.contains("abc123"));
    }

    #[test]
    fn image_source_url() {
        let img = ImageSource::Url {
            url: "https://example.com/img.png".to_string(),
        };
        let json = serde_json::to_string(&img).unwrap();
        assert!(json.contains("url"));
    }

    #[test]
    fn content_block_text() {
        let block = ContentBlock::Text {
            text: "hello".to_string(),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("text"));
    }

    #[test]
    fn content_block_tool_use() {
        let block = ContentBlock::ToolUse {
            id: "tu1".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({"command": "ls"}),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("tool_use"));
    }

    #[test]
    fn content_block_tool_result() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "tu1".to_string(),
            content: "output".to_string(),
            is_error: false,
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("tool_result"));
    }

    #[test]
    fn message_user() {
        let msg = Message::user("hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.text(), "hello");
    }

    #[test]
    fn message_user_with_blocks() {
        let msg = Message::user_with_blocks(vec![
            ContentBlock::Text {
                text: "hi".to_string(),
            },
            ContentBlock::Text {
                text: " there".to_string(),
            },
        ]);
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.text(), "hi there");
    }

    #[test]
    fn message_assistant() {
        let msg = Message::assistant("reply");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.text(), "reply");
    }

    #[test]
    fn message_tool_result() {
        let msg = Message::tool_result("tu1", "output", false);
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.text(), "");
    }

    #[test]
    fn message_text_skips_non_text_blocks() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "a".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "x".to_string(),
                    name: "y".to_string(),
                    input: Value::Null,
                },
                ContentBlock::Text {
                    text: "b".to_string(),
                },
            ],
        };
        assert_eq!(msg.text(), "ab");
    }

    #[test]
    fn message_tool_uses() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "a".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "x".to_string(),
                    name: "y".to_string(),
                    input: Value::Null,
                },
                ContentBlock::Text {
                    text: "b".to_string(),
                },
            ],
        };
        let uses = msg.tool_uses();
        assert_eq!(uses.len(), 1);
    }

    #[test]
    fn tool_definition_serde() {
        let def = ToolDefinition {
            name: "bash".to_string(),
            description: "run shell".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let json = serde_json::to_string(&def).unwrap();
        assert!(json.contains("bash"));
    }

    #[test]
    fn provider_config_default() {
        let config = ProviderConfig {
            provider_type: "test".to_string(),
            base_url: None,
            api_key_env: None,
            context_window: None,
            supports_images: None,
            params: None,
            user_agent: None,
            models: std::collections::HashMap::new(),
        };
        assert_eq!(config.provider_type, "test");
    }

    #[test]
    fn model_config_default() {
        let config = ModelConfig {
            context_window: Some(4096),
            supports_images: Some(true),
            params: None,
            user_agent: None,
        };
        assert_eq!(config.context_window, Some(4096));
    }

    #[test]
    fn vision_config_serde() {
        let config = VisionConfig {
            enabled: true,
            provider: "openai".to_string(),
            model: "gpt-4-vision".to_string(),
            prompt: Some("describe".to_string()),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("gpt-4-vision"));
    }

    #[test]
    fn agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.max_iterations, 500);
        assert!(config.model.is_none());
    }

    #[test]
    fn stream_event_variants() {
        let events = [
            StreamEvent::TextDelta("hi".to_string()),
            StreamEvent::ReasoningDelta("think".to_string()),
            StreamEvent::ToolUseStart {
                id: "x".to_string(),
                name: "y".to_string(),
            },
            StreamEvent::ToolInputDelta("{}".to_string()),
            StreamEvent::ToolUseEnd,
            StreamEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
            StreamEvent::Done,
            StreamEvent::Error("oops".to_string()),
        ];
        assert_eq!(events.len(), 8);
    }
}
