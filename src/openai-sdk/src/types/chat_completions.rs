use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::common::{
    JsonSchemaDefinition, PromptCacheRetention, ReasoningEffort, ServiceTier, Verbosity,
};

// Request types

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CreateChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatCompletionRequestMessage>,

    // Token limits
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[deprecated(note = "Use max_completion_tokens instead")]
    pub max_tokens: Option<u32>,

    // Sampling parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,

    // Penalties
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<std::collections::HashMap<String, i32>>,

    // Output control
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopConfiguration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,

    // Tools and functions
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    // Deprecated function calling
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[deprecated(note = "Use tools instead")]
    pub functions: Vec<FunctionDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[deprecated(note = "Use tool_choice instead")]
    pub function_call: Option<FunctionCallChoice>,

    // Logging
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,

    // Multimodal
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioParameters>,

    // Advanced features
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prediction: Option<PredictionContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[deprecated(note = "Use system_fingerprint monitoring instead")]
    pub seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,

    // Web search
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search_options: Option<WebSearchOptions>,

    // Metadata and identification
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<std::collections::HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[deprecated(note = "Use safety_identifier and prompt_cache_key instead")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "role")]
pub enum ChatCompletionRequestMessage {
    #[serde(rename = "system")]
    System {
        content: ChatCompletionTextContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    #[serde(rename = "user")]
    User {
        content: ChatCompletionUserContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        refusal: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(skip_serializing_if = "Option::is_none")]
        audio: Option<AssistantAudioReference>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    #[serde(rename = "tool")]
    Tool {
        content: ChatCompletionTextContent,
        tool_call_id: String,
    },
    #[serde(rename = "developer")]
    Developer {
        content: ChatCompletionTextContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ChatCompletionUserContent {
    String(String),
    Array(Vec<ChatCompletionUserContentPart>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ChatCompletionContentPartText {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ChatCompletionTextContent {
    String(String),
    Array(Vec<ChatCompletionContentPartText>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ChatCompletionUserContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
    #[serde(rename = "input_audio")]
    InputAudio { input_audio: InputAudio },
    #[serde(rename = "file")]
    File { file: FileInput },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InputAudio {
    /// Base64 encoded audio data.
    pub data: String,
    /// The format of the encoded audio data: "wav" or "mp3".
    pub format: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum Tool {
    #[serde(rename = "function")]
    Function { function: FunctionDefinition },
    #[serde(rename = "custom")]
    Custom { custom: CustomToolDefinition },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CustomToolDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<CustomToolFormat>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum CustomToolFormat {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "grammar")]
    Grammar { grammar: CustomToolGrammar },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CustomToolGrammar {
    pub definition: String,
    pub syntax: CustomToolGrammarSyntax,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CustomToolGrammarSyntax {
    #[serde(rename = "lark")]
    Lark,
    #[serde(rename = "regex")]
    Regex,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
    pub strict: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ToolChoice {
    String(String), // "none", "auto", "required"
    Object {
        #[serde(rename = "type")]
        r#type: String,
        function: FunctionChoice,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionChoice {
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ToolCall {
    #[serde(rename = "function")]
    Function { id: String, function: FunctionCall },
    #[serde(rename = "custom")]
    Custom { id: String, custom: CustomToolCall },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CustomToolCall {
    pub name: String,
    pub output: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

// Response types

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CreateChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[deprecated]
    pub system_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: ChatCompletionResponseMessage,
    pub finish_reason: Option<FinishReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<ChatCompletionLogprobs>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatCompletionResponseMessage {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// Reasoning text on responses from endpoints that surface chain-of-
    /// thought via the OpenRouter/llama.cpp/-style `reasoning_content`
    /// field on the assistant message. Native OpenAI Chat Completions
    /// does not return reasoning on this endpoint (see Responses API
    /// instead), so this field stays `None` there. We accept the
    /// alternate names `reasoning` and `reasoning_text` as well, since
    /// different OpenAI-compatible providers spell it differently.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "reasoning",
        alias = "reasoning_text"
    )]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<Annotation>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioResponse>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PromptTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    /// Number of input tokens that were *written* to the prompt cache on
    /// this request. Not part of the standard OpenAI schema; some
    /// OpenAI-compatible providers (notably ones that proxy Anthropic-
    /// shaped pricing) report it here. When present, callers should
    /// subtract it from `cached_tokens` before treating the remainder as
    /// pure cache reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CompletionTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_prediction_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_prediction_tokens: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatCompletionLogprobs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ChatCompletionTokenLogprob>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Vec<ChatCompletionTokenLogprob>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatCompletionTokenLogprob {
    pub token: String,
    pub logprob: f64,
    pub bytes: Option<Vec<u8>>,
    pub top_logprobs: Vec<ChatCompletionTokenLogprob>,
}

// Annotations (e.g. web search URL citations)

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum Annotation {
    #[serde(rename = "url_citation")]
    UrlCitation { url_citation: AnnotationUrlCitation },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AnnotationUrlCitation {
    pub url: String,
    pub title: String,
    pub start_index: u32,
    pub end_index: u32,
}

// Streaming response types

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CreateChatCompletionStreamResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatCompletionStreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[deprecated]
    pub system_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatCompletionStreamChoice {
    pub index: u32,
    pub delta: ChatCompletionStreamResponseDelta,
    pub finish_reason: Option<FinishReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<ChatCompletionLogprobs>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatCompletionStreamResponseDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// Streaming reasoning chunks from endpoints that surface chain-of-
    /// thought on Chat Completions. Same compatibility notes as
    /// [`ChatCompletionResponseMessage::reasoning_content`]: native
    /// OpenAI does not emit it, but several compatible providers do,
    /// under one of three field names that we coalesce here.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "reasoning",
        alias = "reasoning_text"
    )]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCallDelta {
    pub index: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomToolCallDelta>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CustomToolCallDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionCallDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

// Error types are in common.rs

// Additional type definitions for the complete CreateChatCompletionRequest

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ResponseFormat {
    Text {
        #[serde(rename = "type")]
        r#type: String, // "text"
    },
    JsonObject {
        #[serde(rename = "type")]
        r#type: String, // "json_object"
    },
    JsonSchema {
        #[serde(rename = "type")]
        r#type: String, // "json_schema"
        json_schema: JsonSchemaDefinition,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum StopConfiguration {
    String(String),
    Array(Vec<String>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_obfuscation: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum FunctionCallChoice {
    String(String), // "none", "auto"
    Object { name: String },
}

/// Reference to a previous audio response, for multi-turn audio conversations.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AssistantAudioReference {
    /// Unique identifier for a previous audio response from the model.
    pub id: String,
}

/// Audio response data from the model when using `modalities: ["audio"]`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AudioResponse {
    /// Unique identifier for this audio response.
    pub id: String,
    /// The Unix timestamp (in seconds) for when this audio response expires.
    pub expires_at: u64,
    /// Base64 encoded audio bytes generated by the model.
    pub data: String,
    /// Transcript of the audio generated by the model.
    pub transcript: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AudioParameters {
    pub voice: String,  // "alloy", "ash", "ballad", etc.
    pub format: String, // "wav", "aac", "mp3", "flac", "opus", "pcm16"
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PredictionContent {
    #[serde(rename = "type")]
    pub r#type: String,
    pub content: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WebSearchOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_location: Option<WebSearchLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_context_size: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WebSearchLocation {
    #[serde(rename = "type")]
    pub r#type: String, // "approximate"
    pub approximate: WebSearchLocationApproximate,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WebSearchLocationApproximate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// `finish_reason` reported by Chat Completions and OpenAI-compatible
/// providers. We enumerate the values that AJ classifies explicitly
/// (per §10.3 of the models spec) and keep an `Other(String)` catch-
/// all so that a new or vendor-specific value never breaks the parser.
/// The catch-all preserves the raw wire string so callers can include
/// it in error messages or fold it into `Unknown` categorization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
    /// Some OpenAI-compatible endpoints terminate a stream with
    /// `finish_reason: "network_error"` to signal a recoverable
    /// upstream failure mid-response.
    NetworkError,
    /// Any other `finish_reason` value the upstream emits. The wrapped
    /// string is the verbatim value from the wire.
    Other(String),
}

impl FinishReason {
    /// Wire value as it appears in JSON (`finish_reason: "stop"`,
    /// `"length"`, ...). Inverse of [`FinishReason::from_wire`].
    pub fn as_wire(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::FunctionCall => "function_call",
            Self::NetworkError => "network_error",
            Self::Other(s) => s.as_str(),
        }
    }

    /// Parse a `finish_reason` wire value into the enum. Unrecognised
    /// values are preserved via [`FinishReason::Other`] rather than
    /// rejected, so a future server-side addition can't break the
    /// stream parser.
    pub fn from_wire(value: &str) -> Self {
        match value {
            "stop" | "end" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            "function_call" => Self::FunctionCall,
            "network_error" => Self::NetworkError,
            other => Self::Other(other.to_string()),
        }
    }
}

impl Serialize for FinishReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for FinishReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&raw))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Role {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "tool")]
    Tool,
    #[serde(rename = "function")]
    Function,
    #[serde(rename = "developer")]
    Developer,
}

// Verbosity, PromptCacheRetention, ServiceTier are in common.rs

impl FinishReason {
    /// Returns true if the generation completed naturally (not due to length limit or filter)
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Stop | Self::ToolCalls | Self::FunctionCall)
    }

    /// Returns true if the generation was truncated due to token limits
    pub fn is_truncated(&self) -> bool {
        matches!(self, Self::Length)
    }

    /// Returns true if the generation was filtered by content policies
    pub fn is_filtered(&self) -> bool {
        matches!(self, Self::ContentFilter)
    }
}

// Convenience implementations

impl CreateChatCompletionRequest {
    pub fn new(model: String, messages: Vec<ChatCompletionRequestMessage>) -> Self {
        Self {
            model,
            messages,
            ..Default::default()
        }
    }
}

impl ChatCompletionRequestMessage {
    pub fn system(content: String) -> Self {
        Self::System {
            content: ChatCompletionTextContent::String(content),
            name: None,
        }
    }

    pub fn user(content: String) -> Self {
        Self::User {
            content: ChatCompletionUserContent::String(content),
            name: None,
        }
    }

    pub fn assistant(content: String) -> Self {
        Self::Assistant {
            content: Some(content),
            refusal: None,
            tool_calls: Vec::new(),
            audio: None,
            name: None,
        }
    }

    pub fn assistant_with_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self::Assistant {
            content: None,
            refusal: None,
            tool_calls,
            audio: None,
            name: None,
        }
    }

    pub fn tool(content: String, tool_call_id: String) -> Self {
        Self::Tool {
            content: ChatCompletionTextContent::String(content),
            tool_call_id,
        }
    }

    pub fn developer(content: String) -> Self {
        Self::Developer {
            content: ChatCompletionTextContent::String(content),
            name: None,
        }
    }
}

impl ChatCompletionUserContent {
    pub fn text(text: String) -> Self {
        Self::String(text)
    }

    pub fn with_image(text: String, image_url: String) -> Self {
        Self::Array(vec![
            ChatCompletionUserContentPart::Text { text },
            ChatCompletionUserContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: image_url,
                    detail: None,
                },
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reason_round_trips_known_values() {
        for (json, expected) in [
            ("\"stop\"", FinishReason::Stop),
            ("\"length\"", FinishReason::Length),
            ("\"tool_calls\"", FinishReason::ToolCalls),
            ("\"content_filter\"", FinishReason::ContentFilter),
            ("\"function_call\"", FinishReason::FunctionCall),
            ("\"network_error\"", FinishReason::NetworkError),
        ] {
            let parsed: FinishReason = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, expected, "decode {json}");
            let encoded = serde_json::to_string(&parsed).unwrap();
            assert_eq!(encoded, json, "round-trip {json}");
        }
    }

    #[test]
    fn finish_reason_normalises_end_alias_to_stop() {
        // Some OpenAI-compatible endpoints emit "end" rather than "stop".
        // §7.2 of the models spec maps both to the same `Stop`.
        let parsed: FinishReason = serde_json::from_str("\"end\"").unwrap();
        assert_eq!(parsed, FinishReason::Stop);
    }

    #[test]
    fn finish_reason_falls_back_to_other_for_unknown() {
        let parsed: FinishReason = serde_json::from_str("\"some_new_reason\"").unwrap();
        assert_eq!(parsed, FinishReason::Other("some_new_reason".to_string()));
        // Round-trips back to the same wire string.
        let encoded = serde_json::to_string(&parsed).unwrap();
        assert_eq!(encoded, "\"some_new_reason\"");
    }

    #[test]
    fn stream_delta_parses_reasoning_content_field() {
        let json = r#"{"role":"assistant","reasoning_content":"thinking..."}"#;
        let delta: ChatCompletionStreamResponseDelta = serde_json::from_str(json).unwrap();
        assert_eq!(delta.reasoning_content.as_deref(), Some("thinking..."));
    }

    #[test]
    fn stream_delta_accepts_reasoning_alias() {
        // Some OpenAI-compatible providers expose chain-of-thought via
        // `reasoning` rather than `reasoning_content`.
        let json = r#"{"reasoning":"alt name"}"#;
        let delta: ChatCompletionStreamResponseDelta = serde_json::from_str(json).unwrap();
        assert_eq!(delta.reasoning_content.as_deref(), Some("alt name"));
    }

    #[test]
    fn response_message_parses_reasoning_content_field() {
        let json = r#"{"role":"assistant","content":"hi","reasoning_text":"thinking"}"#;
        let msg: ChatCompletionResponseMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.content.as_deref(), Some("hi"));
        assert_eq!(msg.reasoning_content.as_deref(), Some("thinking"));
    }

    #[test]
    fn prompt_tokens_details_parses_cache_write_tokens() {
        let json = r#"{"cached_tokens":120,"cache_write_tokens":30}"#;
        let details: PromptTokensDetails = serde_json::from_str(json).unwrap();
        assert_eq!(details.cached_tokens, Some(120));
        assert_eq!(details.cache_write_tokens, Some(30));
    }

    #[test]
    fn prompt_tokens_details_omits_cache_write_when_absent() {
        let details = PromptTokensDetails {
            audio_tokens: None,
            cached_tokens: Some(50),
            cache_write_tokens: None,
        };
        let encoded = serde_json::to_string(&details).unwrap();
        assert!(
            !encoded.contains("cache_write_tokens"),
            "cache_write_tokens should be skipped when None: {encoded}"
        );
    }
}
