//! Built-in tool implementations for AJ.
//!
//! Tools implement [`aj_agent::tool::ToolDefinition`] (per
//! `docs/aj-next-plan.md` §1.2 / §1.3) and convert into the
//! type-erased [`aj_agent::tool::ErasedToolDefinition`] for storage
//! in the agent's heterogeneous tool collection. The agent drives
//! them directly — there is no longer a legacy bridge layer in
//! between.
//!
//! Rendering of [`aj_agent::tool::ToolDetails`] payloads onto a
//! terminal lives in the binary that subscribes to the agent's bus
//! ([`AgentEvent::ToolExecutionEnd`](aj_agent::events::AgentEvent::ToolExecutionEnd)
//! carries the structured result); `aj-tools` is wire-only.

pub mod sanitize;
pub mod testing;
pub mod tools;

pub use sanitize::sanitize_terminal_output;

use aj_agent::tool::ErasedToolDefinition;

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
