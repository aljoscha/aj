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
    pub container: Option<ContainerParam>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<MCPServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<Speed>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_profile_id: Option<String>,
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
        #[serde(default)]
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
        content: WebSearchToolResultContent,
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
        content: McpToolResultContent,
        is_error: bool,
    },
    #[serde(rename = "container_upload")]
    ContainerUploadBlock { file_id: String },
    /// Reference to a tool discovered via tool search. Used as content
    /// inside a custom `tool_result` (for client-side tool search) or
    /// nested inside a server-side `tool_search_tool_result`.
    #[serde(rename = "tool_reference")]
    ToolReference { tool_name: String },
    /// A compaction block produced by autocompact. Clients should
    /// round-trip these (including `encrypted_content`) verbatim.
    /// `content = None` means compaction failed; the server treats
    /// these as no-ops.
    #[serde(rename = "compaction")]
    CompactionBlock {
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
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

/// Content of an MCP tool result: either a plain string or an array of
/// text-shaped content blocks.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum McpToolResultContent {
    Text(String),
    Blocks(Vec<Value>),
}

impl From<String> for McpToolResultContent {
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
    Content { content: DocumentContentSource },
    #[serde(rename = "file")]
    File { file_id: String },
}

/// Content for a document whose `source.type` is `"content"`: either an
/// inline string or an array of text/image content blocks.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum DocumentContentSource {
    Text(String),
    Blocks(Vec<DocumentContentBlock>),
}

impl From<String> for DocumentContentSource {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

/// A content block nested inside a `content`-shaped document source.
/// Only text and image blocks are permitted here.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum DocumentContentBlock {
    #[serde(rename = "text")]
    TextBlock {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        citations: Option<Vec<Citation>>,
    },
    #[serde(rename = "image")]
    ImageBlock { source: ImageSource },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        start_char_index: u64,
    },
    #[serde(rename = "page_location")]
    PageLocation {
        cited_text: String,
        document_index: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        document_title: Option<String>,
        end_page_number: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        start_page_number: u64,
    },
    #[serde(rename = "content_block_location")]
    ContentBlockLocation {
        cited_text: String,
        document_index: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        document_title: Option<String>,
        end_block_index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
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

/// The `content` of a `web_search_tool_result` block: either a single bare
/// error object or a list of result blocks. Matches the upstream shape
/// `Union[WebSearchToolResultError, List[WebSearchResultBlock]]`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum WebSearchToolResultContent {
    Error(WebSearchToolResultError),
    Results(Vec<WebSearchResultBlock>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WebSearchToolResultError {
    #[serde(rename = "type")]
    pub r#type: WebSearchToolResultErrorType,
    pub error_code: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub enum WebSearchToolResultErrorType {
    #[default]
    #[serde(rename = "web_search_tool_result_error")]
    WebSearchToolResultError,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WebSearchResultBlock {
    #[serde(rename = "type")]
    pub r#type: WebSearchResultBlockType,
    pub encrypted_content: String,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_age: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub enum WebSearchResultBlockType {
    #[default]
    #[serde(rename = "web_search_result")]
    WebSearchResult,
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
/// Defines an MCP server connection. The toolset-based configuration via
/// `Tool` entries in the `tools` array is preferred, but the legacy
/// per-server `tool_configuration` is also supported.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MCPServer {
    pub name: String,
    pub r#type: MCPServerType,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_configuration: Option<MCPServerToolConfiguration>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MCPServerType {
    #[serde(rename = "url")]
    Url,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MCPServerToolConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Request-side `container` parameter: either a bare container ID or a
/// config struct optionally specifying skills to load.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ContainerParam {
    Id(String),
    Config(ContainerConfig),
}

impl From<String> for ContainerParam {
    fn from(id: String) -> Self {
        Self::Id(id)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ContainerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<SkillParam>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SkillParam {
    pub skill_id: String,
    pub r#type: SkillType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<ContextManagementResponse>,
}

/// Structured details about why the model stopped (currently only refusal).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum StopDetails {
    #[serde(rename = "refusal")]
    Refusal {
        /// Policy category, e.g. `"cyber"` or `"bio"`.
        #[serde(skip_serializing_if = "Option::is_none")]
        category: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
    },
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
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
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
        content: WebSearchToolResultContent,
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
        content: McpToolResultContent,
        is_error: bool,
        tool_use_id: String,
    },
    #[serde(rename = "container_upload")]
    ContainerUploadBlock { file_id: String },
    #[serde(rename = "thinking")]
    ThinkingBlock { signature: String, thinking: String },
    #[serde(rename = "redacted_thinking")]
    RedactedThinkingBlock { data: String },
    /// A compaction block returned when autocompact fires.
    /// `content = None` means compaction failed; the server treats these
    /// as no-ops.
    #[serde(rename = "compaction")]
    CompactionBlock {
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
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
            ContentBlock::ToolUseBlock {
                id,
                input,
                name,
                caller,
            } => ContentBlockParam::ToolUseBlock {
                id,
                input,
                name,
                caller,
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
                caller,
            } => ContentBlockParam::WebSearchToolResultBlock {
                content,
                tool_use_id,
                caller,
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
            ContentBlock::CompactionBlock {
                content,
                encrypted_content,
            } => ContentBlockParam::CompactionBlock {
                content,
                encrypted_content,
            },
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
    #[serde(rename = "compaction")]
    Compaction,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Vec<UsageIteration>>,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<Speed>,
}

impl Usage {
    /// Merge a cumulative [`UsageDelta`] (as carried by a streaming usage
    /// update event) into this usage. Fields present in the delta REPLACE
    /// the corresponding fields here; absent fields are preserved.
    pub fn apply_delta(&mut self, delta: &UsageDelta) {
        if let Some(tokens) = delta.cache_creation_input_tokens {
            self.cache_creation_input_tokens = Some(tokens);
        }
        if let Some(tokens) = delta.cache_read_input_tokens {
            self.cache_read_input_tokens = Some(tokens);
        }
        if let Some(input_tokens) = delta.input_tokens {
            self.input_tokens = input_tokens;
        }
        if let Some(iterations) = delta.iterations.as_ref() {
            self.iterations = Some(iterations.clone());
        }
        if let Some(output_tokens) = delta.output_tokens {
            self.output_tokens = output_tokens;
        }
        if let Some(server_tool_use) = delta.server_tool_use.as_ref() {
            self.server_tool_use = Some(server_tool_use.clone());
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
                    existing.web_fetch_requests += other_server_tool_use.web_fetch_requests;
                }
                None => {
                    self.server_tool_use = Some(other_server_tool_use.clone());
                }
            }
        }
        if let Some(service_tier) = other.service_tier.as_ref() {
            self.service_tier = Some(service_tier.clone());
        }
        if let Some(speed) = other.speed.as_ref() {
            self.speed = Some(speed.clone());
        }
        if let Some(other_iterations) = other.iterations.as_ref() {
            self.iterations
                .get_or_insert_with(Vec::new)
                .extend(other_iterations.iter().cloned());
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
    #[serde(default)]
    pub web_fetch_requests: u64,
}

/// Per-iteration token usage for a single sampling iteration within a
/// request (e.g. server-side tool loops, autocompact, advisor calls).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum UsageIteration {
    #[serde(rename = "message")]
    Message {
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_creation: Option<CacheCreation>,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
    },
    #[serde(rename = "compaction")]
    Compaction {
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_creation: Option<CacheCreation>,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
    },
    #[serde(rename = "advisor_message")]
    AdvisorMessage {
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_creation: Option<CacheCreation>,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
        model: String,
    },
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

/// Inference speed mode (beta `speed` request parameter and the
/// `speed` field the API reports back in Usage).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Speed {
    #[serde(rename = "standard")]
    Standard,
    #[serde(rename = "fast")]
    Fast,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Container {
    pub expires_at: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<Skill>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Skill {
    pub skill_id: String,
    pub r#type: SkillType,
    pub version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SkillType {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "custom")]
    Custom,
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
    #[serde(rename = "compaction_delta")]
    CompactionDelta {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
}

/// Incremental usage update carried on a `message_delta`-style streaming
/// event. Token counts are cumulative; only the subset of [`Usage`] fields
/// that actually change mid-stream is modeled here. Fields like
/// `cache_creation`, `service_tier`, and `speed` arrive once on the
/// initial message-start event and do not update.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UsageDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Vec<UsageIteration>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
}

/// Response-side summary of which context-management edits fired during
/// a request.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ContextManagementResponse {
    pub applied_edits: Vec<AppliedEdit>,
}

/// A single context-management edit that fired, with counts of what it
/// cleared.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum AppliedEdit {
    #[serde(rename = "clear_tool_uses_20250919")]
    ClearToolUses20250919 {
        cleared_input_tokens: u64,
        cleared_tool_uses: u64,
    },
    #[serde(rename = "clear_thinking_20251015")]
    ClearThinking20251015 {
        cleared_input_tokens: u64,
        cleared_thinking_turns: u64,
    },
}
