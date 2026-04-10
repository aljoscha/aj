//! Provider-independent unified message types.
//!
//! All provider implementations produce and consume these types. They are the
//! canonical representation for conversations, tool calls, and streaming
//! options across Anthropic, OpenAI, and any future providers.
//!
//! See `docs/models-spec.md` §1 and §4 for the full design.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// §1.1 Content Types
// ---------------------------------------------------------------------------

/// Text content block.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TextContent {
    pub text: String,
    /// Opaque provider-specific signature for multi-turn replay.
    /// Anthropic: unused. OpenAI Responses: JSON-encoded TextSignatureV1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

/// Extended thinking / reasoning content.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThinkingContent {
    pub thinking: String,
    /// Opaque signature for multi-turn replay. Anthropic: base64 signature,
    /// OpenAI: reasoning item JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// When true, content was redacted by safety filters. The encrypted
    /// payload is in `thinking_signature` for multi-turn continuity.
    #[serde(default)]
    pub redacted: bool,
}

/// Base64-encoded image content.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ImageContent {
    /// Base64-encoded image data.
    pub data: String,
    /// MIME type, e.g. "image/png".
    pub mime_type: String,
}

/// A tool invocation requested by the model.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCall {
    /// Provider-assigned tool call ID, used to match results.
    pub id: String,
    /// Tool name (must match a `ToolDefinition.name`).
    pub name: String,
    /// Parsed JSON arguments for the tool.
    pub arguments: Value,
}

/// Content that can appear in an assistant message.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum AssistantContent {
    #[serde(rename = "text")]
    Text(TextContent),
    #[serde(rename = "thinking")]
    Thinking(ThinkingContent),
    #[serde(rename = "tool_call")]
    ToolCall(ToolCall),
}

/// Content that can appear in a user message.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum UserContent {
    #[serde(rename = "text")]
    Text(TextContent),
    #[serde(rename = "image")]
    Image(ImageContent),
}

// ---------------------------------------------------------------------------
// §1.2 Messages
// ---------------------------------------------------------------------------

/// A message from the user.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserMessage {
    pub content: Vec<UserContent>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A message from the assistant (model response).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    /// Which API produced this message (e.g. "anthropic-messages",
    /// "openai-completions").
    pub api: String,
    /// Which provider (e.g. "anthropic", "openai").
    pub provider: String,
    /// Exact model ID used.
    pub model: String,
    /// Provider-specific response/message ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    /// Error description when `stop_reason == Error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A tool result returned to the model.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolResultMessage {
    /// ID of the tool call this result corresponds to.
    pub tool_call_id: String,
    /// Name of the tool that was called.
    pub tool_name: String,
    /// Result content — text and/or images.
    pub content: Vec<UserContent>,
    /// Whether the tool execution resulted in an error.
    #[serde(default)]
    pub is_error: bool,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A conversation message — one of user, assistant, or tool result.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "tool_result")]
    ToolResult(ToolResultMessage),
}

// ---------------------------------------------------------------------------
// §1.3 Stop Reason & Usage
// ---------------------------------------------------------------------------

/// Why the model stopped generating.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Normal completion (end of turn).
    Stop,
    /// Hit the maximum token limit.
    Length,
    /// Model requested tool use.
    ToolUse,
    /// An error occurred during generation.
    Error,
    /// Generation was aborted by the user or system.
    Aborted,
}

impl Default for StopReason {
    fn default() -> Self {
        Self::Stop
    }
}

/// Token usage for a single model response.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

/// Dollar costs broken down by token category.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

// ---------------------------------------------------------------------------
// §1.4 Tool Definition
// ---------------------------------------------------------------------------

/// Description of a tool the model can invoke.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool's parameters.
    pub parameters: Value,
}

// ---------------------------------------------------------------------------
// §1.5 Context (input to a streaming call)
// ---------------------------------------------------------------------------

/// Everything the provider needs to make a streaming inference call.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

// ---------------------------------------------------------------------------
// §1.6 Thinking Level
// ---------------------------------------------------------------------------

/// Controls the depth of extended thinking / reasoning.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

// ---------------------------------------------------------------------------
// §4 Stream Options
// ---------------------------------------------------------------------------

/// Prompt cache retention preference.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

/// Options passed to any streaming call.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Prompt cache retention preference.
    pub cache_retention: CacheRetention,
    /// Session ID for providers that support session-based caching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Extra HTTP headers merged with provider defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// Metadata fields (e.g. Anthropic user_id for rate limiting).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
}

/// Higher-level options that include reasoning control.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SimpleStreamOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ThinkingLevel>,
}

// ---------------------------------------------------------------------------
// Convenience constructors
// ---------------------------------------------------------------------------

impl TextContent {
    /// Create a plain text content block with no signature.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            text_signature: None,
        }
    }
}

impl UserContent {
    /// Create a text user content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent::new(text))
    }

    /// Create an image user content block.
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image(ImageContent {
            data: data.into(),
            mime_type: mime_type.into(),
        })
    }
}

impl AssistantContent {
    /// Create a text assistant content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent::new(text))
    }
}

impl UserMessage {
    /// Create a user message with a single text content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![UserContent::text(text)],
            timestamp: 0,
        }
    }
}

impl ToolResultMessage {
    /// Create a text-only tool result.
    pub fn text(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        text: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![UserContent::text(text)],
            is_error,
            timestamp: 0,
        }
    }
}

impl AssistantMessage {
    /// Create a default/empty assistant message (useful as a partial during
    /// streaming).
    pub fn empty() -> Self {
        Self {
            content: Vec::new(),
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::default(),
            error_message: None,
            timestamp: 0,
        }
    }
}

impl Context {
    /// Create a context with a system prompt and no prior messages or tools.
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            system_prompt: Some(system_prompt.into()),
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_roundtrip() {
        // Verify that our Message enum serializes/deserializes correctly.
        let msg = Message::User(UserMessage::text("hello"));
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        match back {
            Message::User(u) => {
                assert_eq!(u.content.len(), 1);
                match &u.content[0] {
                    UserContent::Text(t) => assert_eq!(t.text, "hello"),
                    _ => panic!("expected text content"),
                }
            }
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn test_assistant_message_roundtrip() {
        let msg = Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContent::text("some text"),
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "let me think".into(),
                    thinking_signature: Some("sig123".into()),
                    redacted: false,
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "/tmp/test"}),
                }),
            ],
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            response_id: Some("resp_123".into()),
            usage: Usage {
                input: 100,
                output: 50,
                cache_read: 10,
                cache_write: 5,
                total_tokens: 165,
                cost: UsageCost::default(),
            },
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 1234567890,
        });

        let json = serde_json::to_string_pretty(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        match back {
            Message::Assistant(a) => {
                assert_eq!(a.content.len(), 3);
                assert_eq!(a.stop_reason, StopReason::ToolUse);
                assert_eq!(a.usage.input, 100);
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn test_tool_result_roundtrip() {
        let msg = Message::ToolResult(ToolResultMessage::text(
            "call_1",
            "read_file",
            "file contents here",
            false,
        ));

        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        match back {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id, "call_1");
                assert_eq!(tr.tool_name, "read_file");
                assert!(!tr.is_error);
            }
            _ => panic!("expected ToolResult message"),
        }
    }

    #[test]
    fn test_stop_reason_default() {
        assert_eq!(StopReason::default(), StopReason::Stop);
    }

    #[test]
    fn test_cache_retention_default() {
        assert_eq!(CacheRetention::default(), CacheRetention::Short);
    }

    #[test]
    fn test_stream_options_default() {
        let opts = StreamOptions::default();
        assert_eq!(opts.cache_retention, CacheRetention::Short);
        assert!(opts.temperature.is_none());
        assert!(opts.api_key.is_none());
    }

    #[test]
    fn test_simple_stream_options_flatten() {
        // Verify that SimpleStreamOptions flattens base fields correctly.
        let opts = SimpleStreamOptions {
            base: StreamOptions {
                temperature: Some(0.7),
                ..Default::default()
            },
            reasoning: Some(ThinkingLevel::High),
        };
        let json = serde_json::to_value(&opts).unwrap();
        // temperature should be at the top level due to #[serde(flatten)]
        assert_eq!(json["temperature"], 0.7);
        assert_eq!(json["reasoning"], "High");
    }

    #[test]
    fn test_context_constructor() {
        let ctx = Context::new("You are a helpful assistant.");
        assert_eq!(
            ctx.system_prompt.as_deref(),
            Some("You are a helpful assistant.")
        );
        assert!(ctx.messages.is_empty());
        assert!(ctx.tools.is_empty());
    }
}
