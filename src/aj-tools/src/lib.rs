//! Built-in tool implementations for AJ.
//!
//! The trait surface (`ToolDefinition`, `ToolResult`,
//! `ErasedToolDefinition`, `SessionContext`, `TurnContext`) and the
//! `AjUi` re-export live in `aj_agent::legacy_tool`. This crate now
//! contains only the concrete tool implementations and the
//! [`get_builtin_tools`] catalog. See `docs/aj-next-plan.md` §2.0(c).
//!
//! Tools migrated to the new [`aj_agent::tool::ToolDefinition`] shape
//! (per `docs/aj-next-plan.md` §2.2) are wrapped via [`bridge::legacy_adapt`]
//! so they appear identical to legacy tools at the catalog level until
//! the agent runtime stops driving the legacy contract in §2.4.

// Re-export the legacy contract so existing in-crate `use crate::{...}`
// paths keep working without changes.
pub use aj_agent::legacy_tool::{
    AjUi, ErasedToolDefinition, SessionContext, TokenUsage, ToolDefinition, ToolResult,
    TurnContext, UsageSummary, UserOutput,
};

pub mod bridge;
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
        // Tools migrated to the new `aj_agent::tool::ToolDefinition`
        // shape are wrapped via the bridge so they appear identical to
        // legacy tools at the catalog level. Un-migrated tools below
        // still go through the legacy `.into()` path.
        bridge::legacy_adapt(AgentTool),
        BashTool.into(),
        bridge::legacy_adapt(ReadFileTool),
        bridge::legacy_adapt(WriteFileTool),
        EditFileTool.into(),
        EditFileMultiTool.into(),
        bridge::legacy_adapt(LsTool),
        bridge::legacy_adapt(GlobTool),
        bridge::legacy_adapt(GrepTool),
        bridge::legacy_adapt(TodoReadTool),
        bridge::legacy_adapt(TodoWriteTool),
    ]
}
