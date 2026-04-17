//! A 1-to-1 copy of our Anthropic API message definitions, but meant as our
//! generic model independent API. Let's see how far we get with this when
//! adding support for other providers and models.

use std::fmt::Display;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Messages {
    pub model: String,
    pub messages: Vec<MessageParam>,
    pub max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<MCPServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<ContentBlockParam>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MessageParam {
    pub role: Role,
    pub content: Vec<ContentBlockParam>,
}

impl MessageParam {
    pub fn new_user_message(content: Vec<ContentBlockParam>) -> Self {
        Self {
            role: Role::User,
            content,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Role {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ContentBlockParam {
    #[serde(rename = "text")]
    TextBlock {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        citations: Option<Vec<Citation>>,
        /// Opaque provider-specific signature used for replay.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "image")]
    ImageBlock { source: ImageSource },
    #[serde(rename = "document")]
    DocumentBlock {
        source: DocumentSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        citations: Option<Vec<Citation>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    #[serde(rename = "thinking")]
    ThinkingBlock { signature: String, thinking: String },
    #[serde(rename = "redacted_thinking")]
    RedactedThinkingBlock { data: String },
    #[serde(rename = "tool_use")]
    ToolUseBlock {
        id: String,
        input: Value,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "tool_result")]
    ToolResultBlock {
        tool_use_id: String,
        content: ToolResultContent,
        is_error: bool,
    },
    #[serde(rename = "server_tool_use")]
    ServerToolUseBlock {
        id: String,
        input: Value,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "web_search_tool_result")]
    WebSearchToolResultBlock {
        content: Vec<WebSearchToolResultContent>,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "web_fetch_tool_result")]
    WebFetchToolResultBlock {
        content: Value,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "code_execution_tool_result")]
    CodeExecutionToolResultBlock {
        content: Vec<Value>,
        tool_use_id: String,
    },
    #[serde(rename = "bash_code_execution_tool_result")]
    BashCodeExecutionToolResultBlock { content: Value, tool_use_id: String },
    #[serde(rename = "text_editor_code_execution_tool_result")]
    TextEditorCodeExecutionToolResultBlock { content: Value, tool_use_id: String },
    #[serde(rename = "tool_search_tool_result")]
    ToolSearchToolResultBlock { content: Value, tool_use_id: String },
    #[serde(rename = "advisor_tool_result")]
    AdvisorToolResultBlock { content: Value, tool_use_id: String },
    #[serde(rename = "mcp_tool_use")]
    MCPToolUseBlock {
        id: String,
        input: Value,
        name: String,
        server_name: String,
    },
    #[serde(rename = "mcp_tool_result")]
    MCPToolResultBlock {
        tool_use_id: String,
        content: Vec<Value>,
        is_error: bool,
    },
    #[serde(rename = "container_upload")]
    ContainerUploadBlock { file_id: String },
    /// Reference to a tool discovered via tool search. Used as content
    /// inside a custom `tool_result` (for client-side tool search) or
    /// nested inside a server-side `tool_search_tool_result`.
    #[serde(rename = "tool_reference")]
    ToolReference { tool_name: String },
}

impl ContentBlockParam {
    pub fn new_text_block(text: String) -> Self {
        Self::TextBlock {
            text,
            citations: None,
            signature: None,
        }
    }
}

/// Content of a tool result: either a plain string or an array of content blocks.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlockParam>),
}

impl ToolResultContent {
    /// Extract the text representation, joining block texts with newlines for
    /// the `Blocks` variant.
    pub fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlockParam::TextBlock { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl From<String> for ToolResultContent {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ImageSource {
    #[serde(rename = "base64")]
    Base64 { data: String, media_type: String },
    #[serde(rename = "url")]
    Url { url: String },
    #[serde(rename = "file")]
    File { file_id: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum DocumentSource {
    #[serde(rename = "base64")]
    PdfBase64 { data: String, media_type: String },
    #[serde(rename = "url")]
    PdfUrl { url: String },
    #[serde(rename = "text")]
    PlainText { data: String, media_type: String },
    #[serde(rename = "content")]
    Content { content: Value },
    #[serde(rename = "file")]
    File { file_id: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum Citation {
    #[serde(rename = "char_location")]
    CharLocation {
        cited_text: String,
        document_index: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        document_title: Option<String>,
        end_char_index: u64,
        start_char_index: u64,
    },
    #[serde(rename = "page_location")]
    PageLocation {
        cited_text: String,
        document_index: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        document_title: Option<String>,
        end_page_number: u64,
        start_page_number: u64,
    },
    #[serde(rename = "content_block_location")]
    ContentBlockLocation {
        cited_text: String,
        document_index: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        document_title: Option<String>,
        end_block_index: u64,
        start_block_index: u64,
    },
    #[serde(rename = "web_search_result_location")]
    WebSearchResultLocation {
        cited_text: String,
        encrypted_index: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        url: String,
    },
    #[serde(rename = "search_result_location")]
    SearchResultLocation {
        cited_text: String,
        end_block_index: u64,
        search_result_index: u64,
        source: String,
        start_block_index: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum WebSearchToolResultContent {
    #[serde(rename = "web_search_result")]
    WebSearchResult {
        encrypted_content: String,
        title: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        page_age: Option<String>,
    },
    #[serde(rename = "web_search_tool_result_error")]
    WebSearchError { error_code: String },
}

/// Identifies who invoked a server tool (the model directly, or another tool).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum Caller {
    #[serde(rename = "direct")]
    Direct,
    #[serde(rename = "code_execution_20250825")]
    CodeExecution20250825 { tool_id: String },
    #[serde(rename = "code_execution_20260120")]
    CodeExecution20260120 { tool_id: String },
}

/// MCP server connection definition (new format, `mcp-client-2025-11-20`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MCPServer {
    pub name: String,
    pub r#type: MCPServerType,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_token: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MCPServerType {
    #[serde(rename = "url")]
    Url,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum Thinking {
    #[serde(rename = "enabled")]
    Enabled { budget_tokens: u64 },
    #[serde(rename = "disabled")]
    Disabled,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ToolChoice {
    #[serde(rename = "auto")]
    Auto {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    #[serde(rename = "any")]
    Any {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    #[serde(rename = "tool")]
    Tool {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    #[serde(rename = "none")]
    None,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Message {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: MessageType,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
}

impl Message {
    pub fn into_message_param(self) -> MessageParam {
        let content = self
            .content
            .into_iter()
            .map(|c| c.into_content_block_param())
            .collect();
        MessageParam {
            role: self.role,
            content,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MessageType {
    #[serde(rename = "message")]
    Message,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    TextBlock {
        text: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        citations: Vec<Citation>,
        /// Opaque provider-specific signature used for replay.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "tool_use")]
    ToolUseBlock {
        id: String,
        input: Value,
        name: String,
    },
    #[serde(rename = "server_tool_use")]
    ServerToolUseBlock {
        id: String,
        input: Value,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "web_search_tool_result")]
    WebSearchToolResultBlock {
        content: Vec<WebSearchToolResultContent>,
        tool_use_id: String,
    },
    #[serde(rename = "web_fetch_tool_result")]
    WebFetchToolResultBlock {
        content: Value,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "code_execution_tool_result")]
    CodeExecutionToolResultBlock {
        content: Vec<Value>,
        tool_use_id: String,
    },
    #[serde(rename = "bash_code_execution_tool_result")]
    BashCodeExecutionToolResultBlock { content: Value, tool_use_id: String },
    #[serde(rename = "text_editor_code_execution_tool_result")]
    TextEditorCodeExecutionToolResultBlock { content: Value, tool_use_id: String },
    #[serde(rename = "tool_search_tool_result")]
    ToolSearchToolResultBlock { content: Value, tool_use_id: String },
    #[serde(rename = "advisor_tool_result")]
    AdvisorToolResultBlock { content: Value, tool_use_id: String },
    #[serde(rename = "mcp_tool_use")]
    MCPToolUseBlock {
        id: String,
        input: Value,
        name: String,
        server_name: String,
    },
    #[serde(rename = "mcp_tool_result")]
    MCPToolResultBlock {
        content: Vec<Value>,
        is_error: bool,
        tool_use_id: String,
    },
    #[serde(rename = "container_upload")]
    ContainerUploadBlock { file_id: String },
    #[serde(rename = "tool_reference")]
    ToolReferenceBlock { tool_name: String },
    #[serde(rename = "thinking")]
    ThinkingBlock { signature: String, thinking: String },
    #[serde(rename = "redacted_thinking")]
    RedactedThinkingBlock { data: String },
}

impl ContentBlock {
    pub fn new_text_block(text: String) -> Self {
        Self::TextBlock {
            text,
            citations: Vec::new(),
            signature: None,
        }
    }

    pub fn into_content_block_param(self) -> ContentBlockParam {
        match self {
            Self::TextBlock {
                text,
                citations,
                signature,
            } => ContentBlockParam::TextBlock {
                text,
                citations: if citations.is_empty() {
                    None
                } else {
                    Some(citations)
                },
                signature,
            },
            ContentBlock::ToolUseBlock { id, input, name } => ContentBlockParam::ToolUseBlock {
                id,
                input,
                name,
                caller: None,
            },
            ContentBlock::ServerToolUseBlock {
                id,
                input,
                name,
                caller,
            } => ContentBlockParam::ServerToolUseBlock {
                id,
                input,
                name,
                caller,
            },
            ContentBlock::WebSearchToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::WebSearchToolResultBlock {
                content,
                tool_use_id,
                caller: None,
            },
            ContentBlock::WebFetchToolResultBlock {
                content,
                tool_use_id,
                caller,
            } => ContentBlockParam::WebFetchToolResultBlock {
                content,
                tool_use_id,
                caller,
            },
            ContentBlock::CodeExecutionToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::CodeExecutionToolResultBlock {
                content,
                tool_use_id,
            },
            ContentBlock::BashCodeExecutionToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::BashCodeExecutionToolResultBlock {
                content,
                tool_use_id,
            },
            ContentBlock::TextEditorCodeExecutionToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::TextEditorCodeExecutionToolResultBlock {
                content,
                tool_use_id,
            },
            ContentBlock::ToolSearchToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::ToolSearchToolResultBlock {
                content,
                tool_use_id,
            },
            ContentBlock::AdvisorToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::AdvisorToolResultBlock {
                content,
                tool_use_id,
            },
            ContentBlock::MCPToolUseBlock {
                id,
                input,
                name,
                server_name,
            } => ContentBlockParam::MCPToolUseBlock {
                id,
                input,
                name,
                server_name,
            },
            ContentBlock::MCPToolResultBlock {
                content,
                is_error,
                tool_use_id,
            } => ContentBlockParam::MCPToolResultBlock {
                tool_use_id,
                content,
                is_error,
            },
            ContentBlock::ContainerUploadBlock { file_id } => {
                ContentBlockParam::ContainerUploadBlock { file_id }
            }
            ContentBlock::ToolReferenceBlock { tool_name } => {
                ContentBlockParam::ToolReference { tool_name }
            }
            ContentBlock::ThinkingBlock {
                signature,
                thinking,
            } => ContentBlockParam::ThinkingBlock {
                signature,
                thinking,
            },
            ContentBlock::RedactedThinkingBlock { data } => {
                ContentBlockParam::RedactedThinkingBlock { data }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum StopReason {
    #[serde(rename = "end_turn")]
    EndTurn,
    #[serde(rename = "max_tokens")]
    MaxTokens,
    #[serde(rename = "stop_sequence")]
    StopSequence,
    #[serde(rename = "tool_use")]
    ToolUse,
    #[serde(rename = "pause_turn")]
    PauseTurn,
    #[serde(rename = "refusal")]
    Refusal,
    #[serde(rename = "model_context_window_exceeded")]
    ModelContextWindowExceeded,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

impl Usage {
    /// Merge a cumulative [`UsageDelta`] (as carried by a streaming usage
    /// update event) into this usage. Fields present in the delta REPLACE
    /// the corresponding fields here; absent fields are preserved.
    pub fn apply_delta(&mut self, delta: &UsageDelta) {
        if let Some(cache_creation) = delta.cache_creation.as_ref() {
            self.cache_creation = Some(cache_creation.clone());
        }
        if let Some(tokens) = delta.cache_creation_input_tokens {
            self.cache_creation_input_tokens = Some(tokens);
        }
        if let Some(tokens) = delta.cache_read_input_tokens {
            self.cache_read_input_tokens = Some(tokens);
        }
        if let Some(input_tokens) = delta.input_tokens {
            self.input_tokens = input_tokens;
        }
        if let Some(output_tokens) = delta.output_tokens {
            self.output_tokens = output_tokens;
        }
        if let Some(server_tool_use) = delta.server_tool_use.as_ref() {
            self.server_tool_use = Some(server_tool_use.clone());
        }
        if let Some(service_tier) = delta.service_tier.as_ref() {
            self.service_tier = Some(service_tier.clone());
        }
    }

    /// Accumulate another [`Usage`] into this one, for tracking session-wide
    /// usage across multiple turns. Counters are summed; `service_tier` is
    /// replaced (configuration, not a counter).
    pub fn add_usage(&mut self, other: &Usage) {
        if let Some(other_cache_creation) = other.cache_creation.as_ref() {
            match &mut self.cache_creation {
                Some(existing) => {
                    existing.ephemeral_1h_input_tokens +=
                        other_cache_creation.ephemeral_1h_input_tokens;
                    existing.ephemeral_5m_input_tokens +=
                        other_cache_creation.ephemeral_5m_input_tokens;
                }
                None => {
                    self.cache_creation = Some(other_cache_creation.clone());
                }
            }
        }
        if let Some(tokens) = other.cache_creation_input_tokens {
            self.cache_creation_input_tokens =
                Some(self.cache_creation_input_tokens.unwrap_or(0) + tokens);
        }
        if let Some(tokens) = other.cache_read_input_tokens {
            self.cache_read_input_tokens = Some(self.cache_read_input_tokens.unwrap_or(0) + tokens);
        }
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        if let Some(other_server_tool_use) = other.server_tool_use.as_ref() {
            match &mut self.server_tool_use {
                Some(existing) => {
                    existing.web_search_requests += other_server_tool_use.web_search_requests;
                }
                None => {
                    self.server_tool_use = Some(other_server_tool_use.clone());
                }
            }
        }
        if let Some(service_tier) = other.service_tier.as_ref() {
            self.service_tier = Some(service_tier.clone());
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CacheCreation {
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServerToolUsage {
    #[serde(default)]
    pub web_search_requests: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ServiceTier {
    #[serde(rename = "standard")]
    Standard,
    #[serde(rename = "priority")]
    Priority,
    #[serde(rename = "batch")]
    Batch,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Container {
    pub expires_at: String,
    pub id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Error)]
#[serde(tag = "type")]
pub enum ApiError {
    #[serde(rename = "api_error")]
    ApiError { message: String },
    #[serde(rename = "authentication_error")]
    AuthenticationError { message: String },
    #[serde(rename = "billing_error")]
    BillingError { message: String },
    #[serde(rename = "invalid_request_error")]
    InvalidRequestError { message: String },
    #[serde(rename = "not_found_error")]
    NotFoundError { message: String },
    #[serde(rename = "overloaded_error")]
    OverloadedError { message: String },
    #[serde(rename = "permission_error")]
    PermissionError { message: String },
    #[serde(rename = "rate_limit_error")]
    RateLimitError { message: String },
    #[serde(rename = "timeout_error")]
    GatewayTimeoutError { message: String },
}

impl Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::ApiError { message } => write!(f, "API error: {}", message),
            ApiError::AuthenticationError { message } => {
                write!(f, "Authentication error: {}", message)
            }
            ApiError::BillingError { message } => write!(f, "Billing error: {}", message),
            ApiError::InvalidRequestError { message } => {
                write!(f, "Invalid request error: {}", message)
            }
            ApiError::NotFoundError { message } => write!(f, "Not found error: {}", message),
            ApiError::OverloadedError { message } => write!(f, "API Overloaded error: {}", message),
            ApiError::PermissionError { message } => write!(f, "Permission error: {}", message),
            ApiError::RateLimitError { message } => write!(f, "Rate limit error: {}", message),
            ApiError::GatewayTimeoutError { message } => {
                write!(f, "Gateway timeout error: {}", message)
            }
        }
    }
}

impl ApiError {
    pub fn is_overloaded(&self) -> bool {
        matches!(self, ApiError::OverloadedError { message: _ })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiErrorResponse {
    pub error: ApiError,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MessageDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ContentBlockDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UsageDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}
