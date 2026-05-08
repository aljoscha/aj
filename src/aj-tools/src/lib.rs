//! Built-in tool implementations for AJ.
//!
//! The trait surface (`ToolDefinition`, `ToolResult`,
//! `ErasedToolDefinition`, `SessionContext`, `TurnContext`) and the
//! `AjUi` re-export live in `aj_agent::legacy_tool`. This crate now
//! contains only the concrete tool implementations and the
//! [`get_builtin_tools`] catalog. See `docs/aj-next-plan.md` §2.0(c).

// Re-export the legacy contract so existing in-crate `use crate::{...}`
// paths keep working without changes.
pub use aj_agent::legacy_tool::{
    AjUi, ErasedToolDefinition, SessionContext, TokenUsage, ToolDefinition, ToolResult,
    TurnContext, UsageSummary, UserOutput,
};

pub mod testing;
pub mod tools;

pub use tools::agent::AgentTool;
pub use tools::bash::BashTool;
pub use tools::edit_file::EditFileTool;
pub use tools::edit_file_multi::EditFileMultiTool;
pub use tools::glob::GlobTool;
pub use tools::grep::GrepTool;
pub use tools::ls::LsTool;
pub use tools::read_file::ReadFileTool;
pub use tools::todo::{TodoReadTool, TodoWriteTool};
pub use tools::write_file::WriteFileTool;

/// Build the catalog of every builtin tool, ready for `Agent::new`.
///
/// The binary further filters this list against any tools the user
/// has disabled before handing it to the agent. Sub-agents inherit
/// the filtered list (minus the `agent` tool) by cloning, so this
/// function is called exactly once per process.
pub fn get_builtin_tools() -> Vec<ErasedToolDefinition> {
    vec![
        AgentTool.into(),
        BashTool.into(),
        ReadFileTool.into(),
        WriteFileTool.into(),
        EditFileTool.into(),
        EditFileMultiTool.into(),
        LsTool.into(),
        GlobTool.into(),
        GrepTool.into(),
        TodoReadTool.into(),
        TodoWriteTool.into(),
    ]
}
