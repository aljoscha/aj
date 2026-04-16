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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum FinishReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "tool_calls")]
    ToolCalls,
    #[serde(rename = "content_filter")]
    ContentFilter,
    #[serde(rename = "function_call")]
    FunctionCall,
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
