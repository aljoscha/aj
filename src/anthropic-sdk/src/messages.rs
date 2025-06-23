use std::fmt::Display;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Option::is_none")]
        citations: Option<Vec<Citation>>,
    },
    #[serde(rename = "image")]
    ImageBlock {
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "document")]
    DocumentBlock {
        source: DocumentSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
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
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_result")]
    ToolResultBlock {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        content: String,
        is_error: bool,
    },
    #[serde(rename = "server_tool_use")]
    ServerToolUseBlock {
        id: String,
        input: Value,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "web_search_tool_result")]
    WebSearchToolResultBlock {
        content: Vec<WebSearchToolResultContent>,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "code_execution_tool_result")]
    CodeExecutionToolResultBlock {
        content: Vec<Value>,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "mcp_tool_use")]
    MCPToolUseBlock {
        id: String,
        input: Value,
        name: String,
        server_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "mcp_tool_result")]
    MCPToolResultBlock {
        tool_use_id: String,
        content: Vec<Value>,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "container_upload")]
    ContainerUploadBlock {
        file_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

impl ContentBlockParam {
    pub fn new_text_block(text: String) -> Self {
        Self::TextBlock {
            text,
            cache_control: None,
            citations: None,
        }
    }

    // Sets the given cache control if-and-only-if the content type can take
    // one.
    pub fn set_cache_control(&mut self, cache_control: CacheControl) {
        let cache_control_field = match self {
            ContentBlockParam::TextBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ImageBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::DocumentBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ThinkingBlock { .. } => None,
            ContentBlockParam::RedactedThinkingBlock { .. } => None,
            ContentBlockParam::ToolUseBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ToolResultBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ServerToolUseBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::WebSearchToolResultBlock { cache_control, .. } => {
                Some(cache_control)
            }
            ContentBlockParam::CodeExecutionToolResultBlock { cache_control, .. } => {
                Some(cache_control)
            }
            ContentBlockParam::MCPToolUseBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::MCPToolResultBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ContainerUploadBlock { cache_control, .. } => Some(cache_control),
        };

        if let Some(cache_control_field) = cache_control_field {
            *cache_control_field = Some(cache_control);
        }
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
    // TODO: ContentBlock
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

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum CacheControl {
    #[serde(rename = "ephemeral")]
    Ephemeral {
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MCPServer {
    pub name: String,
    pub r#type: MCPServerType,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_configuration: Option<MCPToolConfiguration>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MCPServerType {
    #[serde(rename = "url")]
    Url,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MCPToolConfiguration {
    allowed_tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
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
    },
    #[serde(rename = "web_search_tool_result")]
    WebSearchToolResultBlock {
        content: Vec<WebSearchToolResultContent>,
        tool_use_id: String,
    },
    #[serde(rename = "code_execution_tool_result")]
    CodeExecutionToolResultBlock {
        content: Vec<Value>,
        tool_use_id: String,
    },
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
        }
    }

    pub fn into_content_block_param(self) -> ContentBlockParam {
        match self {
            Self::TextBlock { text, citations } => ContentBlockParam::TextBlock {
                text,
                citations: if citations.is_empty() {
                    None
                } else {
                    Some(citations)
                },
                cache_control: None,
            },
            ContentBlock::ToolUseBlock { id, input, name } => ContentBlockParam::ToolUseBlock {
                id,
                input,
                name,
                cache_control: None,
            },
            ContentBlock::ServerToolUseBlock { id, input, name } => {
                ContentBlockParam::ServerToolUseBlock {
                    id,
                    input,
                    name,
                    cache_control: None,
                }
            }
            ContentBlock::WebSearchToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::WebSearchToolResultBlock {
                content,
                tool_use_id,
                cache_control: None,
            },
            ContentBlock::CodeExecutionToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::CodeExecutionToolResultBlock {
                content,
                tool_use_id,
                cache_control: None,
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
                cache_control: None,
            },
            ContentBlock::MCPToolResultBlock {
                content,
                is_error,
                tool_use_id,
            } => ContentBlockParam::MCPToolResultBlock {
                tool_use_id,
                content,
                is_error,
                cache_control: None,
            },
            ContentBlock::ContainerUploadBlock { file_id } => {
                ContentBlockParam::ContainerUploadBlock {
                    file_id,
                    cache_control: None,
                }
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
}

#[derive(Serialize, Deserialize, Clone, Debug)]
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

impl Usage {
    pub fn update(&mut self, delta: &UsageDelta) {
        // TODO: most of these would also have to be accumulated, but we only
        // care about input/output tokens right now. Pluswhich, we currently
        // don't even look into most of these and treat them as opaque.
        if let Some(cache_creation) = delta.cache_creation.as_ref() {
            self.cache_creation = Some(cache_creation.clone());
        }
        if let Some(cache_creation_input_tokens) = delta.cache_creation_input_tokens {
            self.cache_creation_input_tokens = Some(cache_creation_input_tokens);
        }
        if let Some(cache_read_input_tokens) = delta.cache_read_input_tokens {
            self.cache_read_input_tokens = Some(cache_read_input_tokens);
        }
        if let Some(input_tokens) = delta.input_tokens {
            self.input_tokens += input_tokens;
        }
        if let Some(output_tokens) = delta.output_tokens {
            self.output_tokens += output_tokens;
        }
        if let Some(server_tool_use) = delta.server_tool_use.as_ref() {
            self.server_tool_use = Some(server_tool_use.clone());
        }
        if let Some(service_tier) = delta.service_tier.as_ref() {
            self.service_tier = Some(service_tier.clone());
        }
    }
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
    expires_at: String,
    id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiError {
    pub r#type: String,
    pub message: String,
}

impl Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "API error ({}): {}", self.r#type, self.message)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiErrorResponse {
    pub error: ApiError,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ServerSentEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: Message },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDelta,
        usage: UsageDelta,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u64,
        content_block: ContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        index: u64,
        delta: ContentBlockDelta,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u64 },
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: ApiError },
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
    cache_creation: Option<CacheCreation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_tool_use: Option<ServerToolUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<ServiceTier>,
}
