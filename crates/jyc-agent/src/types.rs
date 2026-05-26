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
    Base64 {
        media_type: String,
        data: String,
    },
    /// Remote http(s) URL — the provider fetches it.
    Url {
        url: String,
    },
}

/// A content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text content.
    Text {
        text: String,
    },
    /// Image content (multimodal input).
    Image {
        source: ImageSource,
    },
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
    pub fn tool_result(tool_use_id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
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
    ToolUseStart {
        id: String,
        name: String,
    },
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
    #[serde(default)]
    pub small_model: Option<String>,
    /// Provider definitions
    #[serde(default)]
    pub providers: std::collections::HashMap<String, ProviderConfig>,
    /// Maximum agent loop iterations per cycle. Default: 500.
    /// When exceeded, agent sends progress reply, resets counter, and continues.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: None,
            small_model: None,
            providers: std::collections::HashMap::new(),
            max_iterations: default_max_iterations(),
        }
    }
}

fn default_max_iterations() -> usize {
    500
}
