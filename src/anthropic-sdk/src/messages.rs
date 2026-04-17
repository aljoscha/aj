//! Types for the Anthropic Messages API, as described at
//! <https://platform.claude.com/docs/en/api/messages>.
//!
//! Covers the latest API surface including server tools, MCP toolsets,
//! adaptive thinking, output configuration, and structured tool types.

use std::collections::HashMap;
use std::fmt::Display;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Request body
// ---------------------------------------------------------------------------

/// The request body for `POST /v1/messages`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Messages {
    pub model: String,
    pub messages: Vec<MessageParam>,
    pub max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<ContextManagementConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<MCPServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<RequestServiceTier>,
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
    pub tools: Vec<ToolUnion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

// ---------------------------------------------------------------------------
// Message parameters (input turns)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Input content blocks (ContentBlockParam)
// ---------------------------------------------------------------------------

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
        citations: Option<CitationsConfig>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    #[serde(rename = "search_result")]
    SearchResultBlock {
        /// Text content blocks within the search result.
        content: Vec<Value>,
        source: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Option::is_none")]
        citations: Option<CitationsConfig>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "tool_result")]
    ToolResultBlock {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        content: ToolResultContent,
        is_error: bool,
    },
    #[serde(rename = "server_tool_use")]
    ServerToolUseBlock {
        id: String,
        input: Value,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "web_search_tool_result")]
    WebSearchToolResultBlock {
        content: Vec<WebSearchToolResultContent>,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "web_fetch_tool_result")]
    WebFetchToolResultBlock {
        content: Value,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<Caller>,
    },
    #[serde(rename = "code_execution_tool_result")]
    CodeExecutionToolResultBlock {
        content: Vec<Value>,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "bash_code_execution_tool_result")]
    BashCodeExecutionToolResultBlock {
        content: Value,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "text_editor_code_execution_tool_result")]
    TextEditorCodeExecutionToolResultBlock {
        content: Value,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_search_tool_result")]
    ToolSearchToolResultBlock {
        content: Value,
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "advisor_tool_result")]
    AdvisorToolResultBlock {
        content: Value,
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
    /// Reference to a tool discovered via tool search. Used both as content
    /// inside a custom `tool_result` (for client-side tool search) and
    /// nested inside a server-side `tool_search_tool_result`.
    #[serde(rename = "tool_reference")]
    ToolReference {
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// A compaction block returned when autocompact fires. Clients should
    /// round-trip these (including `encrypted_content`) verbatim.
    /// `content = None` means compaction failed; the server treats these
    /// as no-ops.
    #[serde(rename = "compaction")]
    CompactionBlock {
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
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

    /// Sets cache control if the content type supports it.
    pub fn set_cache_control(&mut self, cache_control: CacheControl) {
        let field = match self {
            ContentBlockParam::TextBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ImageBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::DocumentBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::SearchResultBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ThinkingBlock { .. } => None,
            ContentBlockParam::RedactedThinkingBlock { .. } => None,
            ContentBlockParam::ToolUseBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ToolResultBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ServerToolUseBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::WebSearchToolResultBlock { cache_control, .. } => {
                Some(cache_control)
            }
            ContentBlockParam::WebFetchToolResultBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::CodeExecutionToolResultBlock { cache_control, .. } => {
                Some(cache_control)
            }
            ContentBlockParam::BashCodeExecutionToolResultBlock { cache_control, .. } => {
                Some(cache_control)
            }
            ContentBlockParam::TextEditorCodeExecutionToolResultBlock { cache_control, .. } => {
                Some(cache_control)
            }
            ContentBlockParam::ToolSearchToolResultBlock { cache_control, .. } => {
                Some(cache_control)
            }
            ContentBlockParam::AdvisorToolResultBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::MCPToolUseBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::MCPToolResultBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ContainerUploadBlock { cache_control, .. } => Some(cache_control),
            ContentBlockParam::ToolReference { cache_control, .. } => Some(cache_control),
            ContentBlockParam::CompactionBlock { cache_control, .. } => Some(cache_control),
        };

        if let Some(field) = field {
            *field = Some(cache_control);
        }
    }
}

// ---------------------------------------------------------------------------
// Tool result content
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Source types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Citations
// ---------------------------------------------------------------------------

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

/// Configuration for enabling citations on a content block or tool.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CitationsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
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

// ---------------------------------------------------------------------------
// Cache control
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum CacheControl {
    #[serde(rename = "ephemeral")]
    Ephemeral {
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// MCP server definition
// ---------------------------------------------------------------------------

/// Defines an MCP server connection (new format, `mcp-client-2025-11-20`).
/// Tool configuration now lives in `ToolUnion::McpToolset` in the `tools` array.
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

/// Per-tool configuration within an MCP toolset.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct McpToolConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
}

// ---------------------------------------------------------------------------
// Context management (request + response)
// ---------------------------------------------------------------------------

/// Request-side configuration listing context-management edits to apply.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ContextManagementConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<ContextManagementEdit>,
}

/// A single context-management edit. The server evaluates each edit's
/// trigger independently and applies it when fired.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ContextManagementEdit {
    /// Clear older `tool_use` blocks when a trigger fires.
    #[serde(rename = "clear_tool_uses_20250919")]
    ClearToolUses20250919 {
        #[serde(skip_serializing_if = "Option::is_none")]
        clear_at_least: Option<InputTokensClearAtLeast>,
        #[serde(skip_serializing_if = "Option::is_none")]
        clear_tool_inputs: Option<ClearToolInputs>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        exclude_tools: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        keep: Option<ToolUsesKeep>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger: Option<ClearToolUsesTrigger>,
    },
    /// Clear older thinking blocks, keeping a window of recent ones.
    #[serde(rename = "clear_thinking_20251015")]
    ClearThinking20251015 {
        #[serde(skip_serializing_if = "Option::is_none")]
        keep: Option<ClearThinkingKeep>,
    },
    /// Automatically compact older context when the trigger fires.
    #[serde(rename = "compact_20260112")]
    Compact20260112 {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pause_after_compaction: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger: Option<InputTokensTrigger>,
    },
}

/// Minimum tokens that must be cleared for a `clear_tool_uses` edit to
/// fire.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum InputTokensClearAtLeast {
    #[serde(rename = "input_tokens")]
    InputTokens { value: u64 },
}

/// Whether to clear tool inputs: `true`/`false` for all, or a list of
/// tool names to clear.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum ClearToolInputs {
    All(bool),
    ByName(Vec<String>),
}

/// Number of tool uses to retain when clearing.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ToolUsesKeep {
    #[serde(rename = "tool_uses")]
    ToolUses { value: u64 },
}

/// Trigger condition for `clear_tool_uses_20250919`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ClearToolUsesTrigger {
    #[serde(rename = "input_tokens")]
    InputTokens { value: u64 },
    #[serde(rename = "tool_uses")]
    ToolUses { value: u64 },
}

/// Number of thinking turns to retain when clearing.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ClearThinkingKeep {
    #[serde(rename = "thinking_turns")]
    ThinkingTurns { value: u64 },
    #[serde(rename = "all")]
    All,
}

/// Trigger condition for `compact_20260112`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum InputTokensTrigger {
    #[serde(rename = "input_tokens")]
    InputTokens { value: u64 },
}

/// Response-side summary of which context-management edits fired.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ContextManagementResponse {
    pub applied_edits: Vec<AppliedEdit>,
}

/// A single edit that was applied, with counts of what it cleared.
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

// ---------------------------------------------------------------------------
// Thinking configuration
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum Thinking {
    #[serde(rename = "enabled")]
    Enabled {
        budget_tokens: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    #[serde(rename = "disabled")]
    Disabled,
    #[serde(rename = "adaptive")]
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ThinkingDisplay {
    #[serde(rename = "summarized")]
    Summarized,
    #[serde(rename = "omitted")]
    Omitted,
}

// ---------------------------------------------------------------------------
// Tool choice
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Tool definitions (ToolUnion)
// ---------------------------------------------------------------------------

/// All tool types that can be passed in the `tools` array.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ToolUnion {
    /// A user-defined (client-side) tool.
    #[serde(rename = "custom")]
    Custom {
        name: String,
        description: String,
        input_schema: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        eager_input_streaming: Option<bool>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        input_examples: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// Web search server tool.
    #[serde(rename = "web_search_20260209")]
    WebSearch {
        name: WebSearchToolName,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_domains: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        blocked_domains: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_uses: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        user_location: Option<UserLocation>,
    },

    /// Web fetch server tool.
    #[serde(rename = "web_fetch_20260309")]
    WebFetch {
        name: WebFetchToolName,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_domains: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        blocked_domains: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        citations: Option<CitationsConfig>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_content_tokens: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_uses: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        use_cache: Option<bool>,
    },

    /// Code execution server tool.
    #[serde(rename = "code_execution_20260120")]
    CodeExecution {
        name: CodeExecutionToolName,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// Computer use tool (Anthropic-defined, client-executed).
    /// Requires beta header `computer-use-2025-11-24`.
    #[serde(rename = "computer_20251124")]
    ComputerUse {
        name: ComputerToolName,
        display_width_px: u64,
        display_height_px: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        display_number: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        enable_zoom: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        input_examples: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// Advisor server tool — consults a higher-intelligence advisor model
    /// mid-generation. Requires beta header `advisor-tool-2026-03-01`.
    #[serde(rename = "advisor_20260301")]
    Advisor {
        name: AdvisorToolName,
        /// Advisor model ID, e.g. `"claude-opus-4-7"`.
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_uses: Option<u64>,
        /// Enables prompt caching for the advisor's own transcript.
        /// Unlike `cache_control`, this is an on/off switch, not a breakpoint.
        #[serde(skip_serializing_if = "Option::is_none")]
        caching: Option<CacheControl>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// Bash tool (Anthropic-defined, client-executed).
    #[serde(rename = "bash_20250124")]
    Bash {
        name: BashToolName,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        input_examples: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// Text editor tool (Anthropic-defined, client-executed).
    #[serde(rename = "text_editor_20250728")]
    TextEditor {
        name: TextEditorToolName,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        input_examples: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_characters: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// Memory server tool.
    #[serde(rename = "memory_20250818")]
    Memory {
        name: MemoryToolName,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        input_examples: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// BM25 tool search (for large tool sets with deferred loading).
    #[serde(rename = "tool_search_tool_bm25")]
    ToolSearchBm25 {
        name: ToolSearchBm25Name,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// Regex tool search (for large tool sets with deferred loading).
    #[serde(rename = "tool_search_tool_regex")]
    ToolSearchRegex {
        name: ToolSearchRegexName,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[serde(default)]
        allowed_callers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },

    /// MCP toolset — configures which tools from an MCP server to enable.
    /// Requires beta header `mcp-client-2025-11-20`.
    #[serde(rename = "mcp_toolset")]
    McpToolset {
        mcp_server_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        default_config: Option<McpToolConfig>,
        #[serde(skip_serializing_if = "Option::is_none")]
        configs: Option<HashMap<String, McpToolConfig>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

// Fixed name enums for server/Anthropic-defined tools. These ensure the
// `name` field serializes to the one valid string for each tool type.

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum WebSearchToolName {
    #[serde(rename = "web_search")]
    WebSearch,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum WebFetchToolName {
    #[serde(rename = "web_fetch")]
    WebFetch,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum CodeExecutionToolName {
    #[serde(rename = "code_execution")]
    CodeExecution,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum BashToolName {
    #[serde(rename = "bash")]
    Bash,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ComputerToolName {
    #[serde(rename = "computer")]
    Computer,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum AdvisorToolName {
    #[serde(rename = "advisor")]
    Advisor,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum TextEditorToolName {
    #[serde(rename = "str_replace_based_edit_tool")]
    StrReplaceBasedEditTool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MemoryToolName {
    #[serde(rename = "memory")]
    Memory,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ToolSearchBm25Name {
    #[serde(rename = "tool_search_tool_bm25")]
    ToolSearchBm25,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ToolSearchRegexName {
    #[serde(rename = "tool_search_tool_regex")]
    ToolSearchRegex,
}

/// Geographic location hint for web search.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserLocation {
    pub r#type: UserLocationType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum UserLocationType {
    #[serde(rename = "approximate")]
    Approximate,
}

// ---------------------------------------------------------------------------
// Output configuration
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<OutputEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_budget: Option<TokenTaskBudget>,
}

/// User-configurable total token budget across contexts. Callers update
/// `remaining` each turn so the model can scale effort against the budget.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum TokenTaskBudget {
    #[serde(rename = "tokens")]
    Tokens {
        /// Total token budget for the whole session.
        total: u64,
        /// Tokens still available. Defaults to `total` if not provided.
        #[serde(skip_serializing_if = "Option::is_none")]
        remaining: Option<u64>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum OutputEffort {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    #[serde(rename = "max")]
    Max,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum OutputFormat {
    #[serde(rename = "json_schema")]
    JsonSchema { schema: Value },
}

// ---------------------------------------------------------------------------
// Request metadata
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Metadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Request-side service tier
// ---------------------------------------------------------------------------

/// Service tier for the request. Distinct from the response-side
/// [`ServiceTier`] which reports which tier was actually used.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum RequestServiceTier {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "standard_only")]
    StandardOnly,
}

/// Inference speed mode for the request. `Fast` enables higher
/// output-tokens-per-second at some quality cost.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Speed {
    #[serde(rename = "standard")]
    Standard,
    #[serde(rename = "fast")]
    Fast,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Response content blocks
// ---------------------------------------------------------------------------

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
                cache_control: None,
                caller,
            },
            ContentBlock::WebSearchToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::WebSearchToolResultBlock {
                content,
                tool_use_id,
                cache_control: None,
                caller: None,
            },
            ContentBlock::WebFetchToolResultBlock {
                content,
                tool_use_id,
                caller,
            } => ContentBlockParam::WebFetchToolResultBlock {
                content,
                tool_use_id,
                cache_control: None,
                caller,
            },
            ContentBlock::CodeExecutionToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::CodeExecutionToolResultBlock {
                content,
                tool_use_id,
                cache_control: None,
            },
            ContentBlock::BashCodeExecutionToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::BashCodeExecutionToolResultBlock {
                content,
                tool_use_id,
                cache_control: None,
            },
            ContentBlock::TextEditorCodeExecutionToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::TextEditorCodeExecutionToolResultBlock {
                content,
                tool_use_id,
                cache_control: None,
            },
            ContentBlock::ToolSearchToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::ToolSearchToolResultBlock {
                content,
                tool_use_id,
                cache_control: None,
            },
            ContentBlock::AdvisorToolResultBlock {
                content,
                tool_use_id,
            } => ContentBlockParam::AdvisorToolResultBlock {
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
            ContentBlock::ToolReferenceBlock { tool_name } => ContentBlockParam::ToolReference {
                tool_name,
                cache_control: None,
            },
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
                cache_control: None,
            },
        }
    }
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

// ---------------------------------------------------------------------------
// Stop reason
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
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
    /// Merge a cumulative [`UsageDelta`] (from a `message_delta` event) into
    /// this usage. Per the API streaming docs, the token counts in a
    /// `message_delta` event are cumulative, so fields present in the delta
    /// REPLACE the corresponding fields here; absent fields are preserved.
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
        if delta.inference_geo.is_some() {
            self.inference_geo.clone_from(&delta.inference_geo);
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
        if let Some(service_tier) = delta.service_tier.as_ref() {
            self.service_tier = Some(service_tier.clone());
        }
        if let Some(speed) = delta.speed.as_ref() {
            self.speed = Some(speed.clone());
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

/// Per-iteration token usage for a single sampling iteration within the
/// request. Reported on `Usage.iterations` when server-side tool loops or
/// autocompact produce multiple inference passes.
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

/// Response-side service tier (which tier was actually used).
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

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Server-sent event types (streaming)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ServerSentEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: Message },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDelta,
        usage: UsageDelta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_management: Option<ContextManagementResponse>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
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
    #[serde(rename = "citations_delta")]
    CitationsDelta { citation: Citation },
    #[serde(rename = "compaction_delta")]
    CompactionDelta {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
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
    pub inference_geo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Vec<UsageIteration>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<Speed>,
}
