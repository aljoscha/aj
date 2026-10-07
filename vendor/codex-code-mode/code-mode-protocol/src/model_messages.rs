// Modified for the standalone AJ extraction: decouple Codex protocol types and optional transport.
use serde::{Deserialize, Serialize};

/// Model-owned messages for a built-in tool.
#[derive(Debug, Default, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ToolMessage {
    /// Missing or null uses the built-in description; an empty string suppresses its static
    /// text without disabling the tool. Tool-owned runtime guidance is retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Complete JSON Schema encoded as a string. Consumed by Multi-Agent V2 tools, Code Mode wait,
    /// request_user_input_async (the send_user_message_async catalog key), and MCP resource helpers.
    /// Uses the harness's supported schema subset; unrecognized keywords are ignored.
    /// Missing, null, invalid or unsupported structures, or a root without `type: "object"`
    /// retains the harness parameters. Schema semantics must remain API-compatible.
    /// Overrides must declare harness-encrypted properties so their annotations can be retained.
    /// Argument handling is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<String>,
}

/// Model-owned instructions for Code Mode's exec and wait tools.
#[derive(Debug, Default, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct CodeModeToolMessages {
    /// Instructional template supporting `{{ default_exec_yield_time_ms }}` and `{{ image_helper }}`.
    /// Runtime tool declarations are appended. Unknown placeholders remain literal.
    /// Exec accepts raw JavaScript; `parameters` is not consumed and its grammar is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<ToolMessage>,
    /// Complete description and JSON parameter schema, selected independently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<ToolMessage>,
    /// Literal guidance appended when deferred nested tools exist.
    /// Missing or null uses bundled text; an empty string omits the section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_nested_tools_guidance: Option<String>,
    /// Literal shared TypeScript definitions, emitted when Code Mode Only exposes MCP results.
    /// Missing or null uses bundled definitions; an empty string omits the section.
    /// Custom definitions must remain compatible with the generated tool declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_typescript_preamble: Option<String>,
}
