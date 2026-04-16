use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::common::{
    JsonSchemaDefinition, PromptCacheRetention, ReasoningEffort, ServiceTier, Verbosity,
};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CreateResponseRequest {
    pub model: String,
    pub input: ResponseInput,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,

    // Tools
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ResponseTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponseToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    // Token limits
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,

    // Sampling parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    // Reasoning
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,

    // Output configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<ResponseTextConfig>,

    // Context handling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Truncation>,

    // Background execution
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,

    // Multi-turn conversation (previous_response_id and conversation are mutually exclusive)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation: Option<Conversation>,

    // Tool call limits
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,

    // Context management
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_management: Vec<ContextManagement>,

    // Streaming
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<ResponseStreamOptions>,

    // Storage and metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,

    // Extra output data
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<ResponseIncludable>,

    // Prompt template
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<ResponsePrompt>,

    // Logging
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,

    // Identification and caching
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[deprecated(note = "Use safety_identifier and prompt_cache_key instead")]
    pub user: Option<String>,
}

impl Default for CreateResponseRequest {
    fn default() -> Self {
        #[allow(deprecated)]
        Self {
            model: String::new(),
            input: ResponseInput::String(String::new()),
            instructions: None,
            background: None,
            previous_response_id: None,
            conversation: None,
            max_tool_calls: None,
            prompt: None,
            context_management: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            reasoning: None,
            text: None,
            truncation: None,
            stream: None,
            stream_options: None,
            store: None,
            metadata: None,
            include: Vec::new(),
            top_logprobs: None,
            service_tier: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            safety_identifier: None,
            user: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

/// The input to a response: either a simple string or a list of input items.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ResponseInput {
    String(String),
    Items(Vec<ResponseInputItem>),
}

/// An input item in the response request.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ResponseInputItem {
    /// A simple message with role and content.
    #[serde(rename = "message")]
    Message {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: InputRole,
        content: ResponseInputMessageContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<MessagePhase>,
    },

    /// A function tool call from a previous response (for multi-turn replay).
    #[serde(rename = "function_call")]
    FunctionCall {
        id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
    },

    /// The output of a function tool call.
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: String,
        output: FunctionCallOutputContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
    },

    /// A reasoning item from a previous response (for multi-turn replay).
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        summary: Vec<ReasoningSummary>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ReasoningContent>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
    },

    /// Reference to another item by ID.
    #[serde(rename = "item_reference")]
    ItemReference { id: String },
}

/// Roles allowed in response input messages.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum InputRole {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "system")]
    System,
    #[serde(rename = "developer")]
    Developer,
}

/// Content of an input message: either a string or a list of content parts.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ResponseInputMessageContent {
    String(String),
    Array(Vec<ResponseInputContentPart>),
}

/// A content part in an input message.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ResponseInputContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "input_image")]
    InputImage {
        #[serde(skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
    #[serde(rename = "input_file")]
    InputFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        #[serde(default)]
        #[serde(skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<ResponseAnnotation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        logprobs: Option<Vec<ResponseLogprob>>,
    },
    #[serde(rename = "refusal")]
    Refusal { refusal: String },
}

/// The output of a function call: either a plain string or a list of content parts.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum FunctionCallOutputContent {
    String(String),
    Array(Vec<ResponseInputContentPart>),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ImageDetail {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "auto")]
    Auto,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Response {
    pub id: String,
    pub object: String,
    pub created_at: f64,
    pub model: String,
    pub output: Vec<ResponseOutputItem>,
    pub parallel_tool_calls: bool,
    pub tools: Vec<ResponseTool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<ResponsePrompt>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<IncompleteDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ResponseStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<ResponseTextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Truncation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponseToolChoice>,
}

impl Response {
    /// Returns all text output concatenated from message output items.
    pub fn output_text(&self) -> String {
        let mut text = String::new();
        for item in &self.output {
            if let ResponseOutputItem::Message { content, .. } = item {
                for part in content {
                    if let ResponseOutputContent::OutputText { text: t, .. } = part {
                        text.push_str(t);
                    }
                }
            }
        }
        text
    }
}

// ---------------------------------------------------------------------------
// Output items
// ---------------------------------------------------------------------------

/// An output item generated by the model.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ResponseOutputItem {
    /// A text/refusal message from the assistant.
    #[serde(rename = "message")]
    Message {
        id: String,
        content: Vec<ResponseOutputContent>,
        role: String, // Always "assistant"
        status: ItemStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<MessagePhase>,
    },

    /// A function tool call.
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
    },

    /// A web search tool call.
    #[serde(rename = "web_search_call")]
    WebSearchCall {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<WebSearchCallStatus>,
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<Value>,
    },

    /// Reasoning/chain-of-thought content.
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        summary: Vec<ReasoningSummary>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ReasoningContent>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
    },

    /// Catch-all for output item types we don't explicitly model.
    #[serde(untagged)]
    Other(Value),
}

/// Content part of an output message.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ResponseOutputContent {
    /// Text output from the model.
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        #[serde(default)]
        #[serde(skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<ResponseAnnotation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        logprobs: Option<Vec<ResponseLogprob>>,
    },

    /// A refusal from the model.
    #[serde(rename = "refusal")]
    Refusal { refusal: String },
}

// ---------------------------------------------------------------------------
// Message phase
// ---------------------------------------------------------------------------

/// Labels an assistant message as intermediate commentary or the final answer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum MessagePhase {
    #[serde(rename = "commentary")]
    Commentary,
    #[serde(rename = "final_answer")]
    FinalAnswer,
}

// ---------------------------------------------------------------------------
// Annotations
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ResponseAnnotation {
    #[serde(rename = "url_citation")]
    UrlCitation {
        url: String,
        title: String,
        start_index: u32,
        end_index: u32,
    },
    #[serde(rename = "file_citation")]
    FileCitation {
        file_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        index: u32,
    },
    /// Catch-all for annotation types we don't explicitly model.
    #[serde(untagged)]
    Other(Value),
}

// ---------------------------------------------------------------------------
// Logprobs
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseLogprob {
    pub token: String,
    pub logprob: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
    #[serde(default)]
    pub top_logprobs: Vec<ResponseLogprobTop>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseLogprobTop {
    pub token: String,
    pub logprob: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Reasoning types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReasoningSummary {
    pub text: String,
    #[serde(rename = "type")]
    pub r#type: String, // "summary_text"
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReasoningContent {
    pub text: String,
    #[serde(rename = "type")]
    pub r#type: String, // "reasoning_text"
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ResponseTool {
    /// A function tool the model may call.
    #[serde(rename = "function")]
    Function {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parameters: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// A web search tool.
    #[serde(rename = "web_search")]
    WebSearch {
        #[serde(skip_serializing_if = "Option::is_none")]
        search_context_size: Option<SearchContextSize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        user_location: Option<WebSearchUserLocation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filters: Option<WebSearchFilters>,
    },

    /// Catch-all for tool types we don't explicitly model.
    #[serde(untagged)]
    Other(Value),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum SearchContextSize {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum WebSearchCallStatus {
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "searching")]
    Searching,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WebSearchFilters {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WebSearchUserLocation {
    #[serde(rename = "type")]
    pub r#type: String, // "approximate"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool choice
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ResponseToolChoice {
    /// "none", "auto", or "required".
    String(String),
    /// Force a specific function.
    Function {
        #[serde(rename = "type")]
        r#type: String, // "function"
        name: String,
    },
}

// ---------------------------------------------------------------------------
// Context management types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ContextManagement {
    #[serde(rename = "type")]
    pub r#type: String, // Currently only "compaction"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<u32>,
}

// ---------------------------------------------------------------------------
// Stream options
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseStreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_obfuscation: Option<bool>,
}

// ---------------------------------------------------------------------------
// Prompt template types
// ---------------------------------------------------------------------------

/// Reference to a stored prompt template.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponsePrompt {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variables: Option<HashMap<String, Value>>,
}

// ---------------------------------------------------------------------------
// Conversation types
// ---------------------------------------------------------------------------

/// A conversation reference in a request: either a conversation ID string or an object.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum Conversation {
    Id(String),
    Object(ConversationObject),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConversationObject {
    pub id: String,
}

// ---------------------------------------------------------------------------
// Configuration types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Reasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummaryMode>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ReasoningSummaryMode {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "concise")]
    Concise,
    #[serde(rename = "detailed")]
    Detailed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseTextConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponseTextFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ResponseTextFormat {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json_object")]
    JsonObject,
    #[serde(rename = "json_schema")]
    JsonSchema { json_schema: JsonSchemaDefinition },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Truncation {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "disabled")]
    Disabled,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ResponseIncludable {
    #[serde(rename = "reasoning.encrypted_content")]
    ReasoningEncryptedContent,
    #[serde(rename = "reasoning.content")]
    ReasoningContent,
    /// Catch-all for includable values we don't explicitly model.
    #[serde(untagged)]
    Other(String),
}

// ---------------------------------------------------------------------------
// Status and error types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ResponseStatus {
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "queued")]
    Queued,
    #[serde(rename = "incomplete")]
    Incomplete,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ItemStatus {
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "incomplete")]
    Incomplete,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IncompleteDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<ResponseInputTokensDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<ResponseOutputTokensDetails>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseInputTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseOutputTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

// ---------------------------------------------------------------------------
// Streaming event types
// ---------------------------------------------------------------------------

/// A server-sent event from the responses streaming API.
///
/// Events are discriminated by the `type` field. We model the events that AJ
/// consumes and use `Other` as a catch-all for unrecognized event types.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ResponseStreamEvent {
    // -- Response lifecycle --------------------------------------------------
    #[serde(rename = "response.created")]
    ResponseCreated {
        response: Response,
        sequence_number: u64,
    },

    #[serde(rename = "response.in_progress")]
    ResponseInProgress {
        response: Response,
        sequence_number: u64,
    },

    #[serde(rename = "response.queued")]
    ResponseQueued {
        response: Response,
        sequence_number: u64,
    },

    #[serde(rename = "response.completed")]
    ResponseCompleted {
        response: Response,
        sequence_number: u64,
    },

    #[serde(rename = "response.failed")]
    ResponseFailed {
        response: Response,
        sequence_number: u64,
    },

    #[serde(rename = "response.incomplete")]
    ResponseIncomplete {
        response: Response,
        sequence_number: u64,
    },

    // -- Output items --------------------------------------------------------
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        item: ResponseOutputItem,
        output_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        item: ResponseOutputItem,
        output_index: u32,
        sequence_number: u64,
    },

    // -- Content parts -------------------------------------------------------
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        part: ResponseOutputContent,
        item_id: String,
        output_index: u32,
        content_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        part: ResponseOutputContent,
        item_id: String,
        output_index: u32,
        content_index: u32,
        sequence_number: u64,
    },

    // -- Text streaming ------------------------------------------------------
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        delta: String,
        item_id: String,
        output_index: u32,
        content_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        text: String,
        item_id: String,
        output_index: u32,
        content_index: u32,
        sequence_number: u64,
    },

    // -- Text annotation -----------------------------------------------------
    #[serde(rename = "response.output_text.annotation.added")]
    OutputTextAnnotationAdded {
        annotation: ResponseAnnotation,
        annotation_index: u32,
        item_id: String,
        output_index: u32,
        content_index: u32,
        sequence_number: u64,
    },

    // -- Function call arguments streaming ------------------------------------
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        delta: String,
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        arguments: String,
        name: String,
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },

    // -- Reasoning streaming -------------------------------------------------
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta {
        delta: String,
        item_id: String,
        output_index: u32,
        content_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.reasoning_text.done")]
    ReasoningTextDone {
        text: String,
        item_id: String,
        output_index: u32,
        content_index: u32,
        sequence_number: u64,
    },

    // -- Reasoning summary streaming -----------------------------------------
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded {
        part: ReasoningSummary,
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone {
        part: ReasoningSummary,
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        delta: String,
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone {
        text: String,
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },

    // -- Refusal streaming ---------------------------------------------------
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta {
        delta: String,
        item_id: String,
        output_index: u32,
        content_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.refusal.done")]
    RefusalDone {
        refusal: String,
        item_id: String,
        output_index: u32,
        content_index: u32,
        sequence_number: u64,
    },

    // -- Web search lifecycle ------------------------------------------------
    #[serde(rename = "response.web_search_call.in_progress")]
    WebSearchCallInProgress {
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.web_search_call.searching")]
    WebSearchCallSearching {
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },

    #[serde(rename = "response.web_search_call.completed")]
    WebSearchCallCompleted {
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },

    // -- Error event ---------------------------------------------------------
    #[serde(rename = "error")]
    Error {
        code: Option<String>,
        message: String,
        sequence_number: u64,
    },

    // -- Catch-all -----------------------------------------------------------
    /// Any event type we don't explicitly model.
    #[serde(untagged)]
    Other(Value),
}

// ---------------------------------------------------------------------------
// Convenience constructors
// ---------------------------------------------------------------------------

impl CreateResponseRequest {
    pub fn new(model: String, input: ResponseInput) -> Self {
        Self {
            model,
            input,
            ..Default::default()
        }
    }
}

impl ResponseInputItem {
    /// Create a user message with text content.
    pub fn user_text(text: String) -> Self {
        Self::Message {
            id: None,
            role: InputRole::User,
            content: ResponseInputMessageContent::String(text),
            status: None,
            phase: None,
        }
    }

    /// Create a system message with text content.
    pub fn system_text(text: String) -> Self {
        Self::Message {
            id: None,
            role: InputRole::System,
            content: ResponseInputMessageContent::String(text),
            status: None,
            phase: None,
        }
    }

    /// Create a developer message with text content.
    pub fn developer_text(text: String) -> Self {
        Self::Message {
            id: None,
            role: InputRole::Developer,
            content: ResponseInputMessageContent::String(text),
            status: None,
            phase: None,
        }
    }

    /// Create a function call output with a string result.
    pub fn function_call_output(call_id: String, output: String) -> Self {
        Self::FunctionCallOutput {
            call_id,
            output: FunctionCallOutputContent::String(output),
            id: None,
            status: None,
        }
    }
}

impl ResponseTool {
    /// Create a function tool definition.
    pub fn function(name: String, description: Option<String>, parameters: Value) -> Self {
        Self::Function {
            name,
            description,
            parameters: Some(parameters),
            strict: None,
        }
    }
}
