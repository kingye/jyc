//! Core types for the agent system.
//!
//! Inspired by jcode's clean architecture but minimal — only what JYC needs.
//!
//! **Config types** (`ProviderDef`, `ModelDef`, `AiConfig`, `VisionConfig`)
//! live in `jyc_types` and are the single source of truth — `jyc-agent`
//! consumes them directly and does not define its own duplicates.
//! `derive_agent_config` in `service.rs` returns `jyc_types::AiConfig`.

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
        /// Prompt-cache **read** tokens reported by the provider for
        /// this call. For Anthropic, this is `cache_read_input_tokens`;
        /// for every other vendor it's the single `cached_tokens` /
        /// `prompt_cache_hit_tokens` field. `0` when the provider
        /// doesn't surface cache hits (or they're absent from the
        /// `usage` JSON). See `provider::usage` for the per-vendor
        /// field mapping.
        cache_hit_tokens: u64,
        /// Prompt-cache **creation** (write) tokens. Anthropic is the
        /// only vendor that reports writes separately from reads; for
        /// every other provider this is `0`. Billed at the dedicated
        /// `cache_creation_per_million` rate when configured, otherwise
        /// folded into `cache_hit_per_million` (parity with the
        /// pre-split model).
        cache_creation_tokens: u64,
        /// Reasoning (thinking) tokens reported by the provider for this
        /// call — the hidden chain-of-thought portion of the output.
        /// OpenAI GPT-5.x/o-series report it via
        /// `completion_tokens_details.reasoning_tokens` (Chat Completions)
        /// or `output_tokens_details.reasoning_tokens` (Responses API);
        /// `0` for providers that don't break it out. These tokens are
        /// already included in (and billed as) `output_tokens` — this
        /// field is informational only.
        reasoning_tokens: u64,
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
    /// Whether the reply was auto-delivered in the agent's name: the model
    /// finished text-only without calling `jyc_reply_message`, so the loop
    /// executed it synthetically with the final text. Surfaces to metrics
    /// so this degradation rate stays measurable separately from real tool
    /// calls.
    pub reply_auto_delivered: bool,
    /// Reply text extracted from the reply_message tool call (if used).
    pub reply_text_from_tool: Option<String>,
    /// Input tokens from the last LLM call in this round (= current
    /// context size). For the cumulative sum across calls in this round,
    /// see `total_input_tokens`.
    pub input_tokens: u64,
    /// Total output tokens used across all turns in this round.
    pub output_tokens: u64,
    /// Accumulated input tokens across all LLM calls in this round.
    /// Each call's `input_tokens` (= full context size) is summed via
    /// `+=`. Pairs with `context_input_tokens` in `SessionState` for
    /// per-round accumulation that survives across multiple rounds
    /// through `update_tokens` / `persist_tokens`.
    pub total_input_tokens: u64,
    /// Accumulated prompt-cache-hit tokens across all LLM calls in
    /// this round. Each call's `cache_hit_tokens` is summed via `+=`.
    /// `0` when no provider in the round surfaced cache hits. Mirrors
    /// `total_input_tokens` in `SessionState` for per-round
    /// accumulation.
    ///
    /// **For Anthropic**, `cache_hit_tokens` carries the cache-**read**
    /// bucket only; cache writes accumulate in
    /// [`total_cache_creation_tokens`](#field.total_cache_creation_tokens).
    /// For every other provider (OpenAI / DeepSeek / Kimi / 火山引擎 /
    /// MiniMax) this is the single reported cache bucket.
    pub total_cache_hit_tokens: u64,
    /// Accumulated prompt-cache-**creation** (write) tokens across
    /// all LLM calls in this round. Anthropic is the only provider
    /// that reports writes separately from reads; for every other
    /// vendor this is `0`. Billed at `cache_creation_per_million`
    /// when configured, otherwise folded into the read rate.
    pub total_cache_creation_tokens: u64,
    /// Accumulated reasoning (thinking) tokens across all LLM calls in
    /// this round — the hidden chain-of-thought share of the output.
    /// Already included in (and billed as) `output_tokens`;
    /// informational only. `0` when no provider broke it out.
    pub total_reasoning_tokens: u64,
    /// The full conversation history (internal format for logic).
    pub history: Vec<Message>,
    /// Raw provider-formatted context (for persistence in agent-context.json).
    /// This preserves provider-specific fields like DeepSeek's reasoning_content.
    pub raw_context: Vec<serde_json::Value>,
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
                cache_hit_tokens: 0,
                cache_creation_tokens: 0,
                reasoning_tokens: 0,
            },
            StreamEvent::Done,
            StreamEvent::Error("oops".to_string()),
        ];
        assert_eq!(events.len(), 8);
    }
}
