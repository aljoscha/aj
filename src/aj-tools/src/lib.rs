//! Built-in tool implementations for AJ.
//!
//! Tools implement [`aj_agent::tool::ToolDefinition`] and convert into
//! the type-erased [`aj_agent::tool::ErasedToolDefinition`] for storage
//! in the agent's heterogeneous tool collection. The agent drives them
//! directly.
//!
//! Rendering of [`aj_agent::tool::ToolDetails`] payloads onto a
//! terminal lives in the binary that subscribes to the agent's bus
//! ([`AgentEvent::ToolExecutionEnd`](aj_agent::events::AgentEvent::ToolExecutionEnd)
//! carries the structured result); `aj-tools` is wire-only.

pub mod image;
pub mod sanitize;
/// Test-only [`aj_agent::tool::ToolContext`] doubles for exercising tools
/// without a live agent runtime. Gated behind `cfg(test)` plus the `testing`
/// feature so it never ships in the production public API. Other crates'
/// tests opt in via `aj-tools = { features = ["testing"] }` in dev-deps.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod tools;
pub mod truncate;

pub use sanitize::sanitize_terminal_output;

use aj_agent::tool::ErasedToolDefinition;

pub use tools::agent::AgentTool;
pub use tools::bash::BashTool;
pub use tools::edit_file::EditFileTool;
pub use tools::edit_file_multi::EditFileMultiTool;
pub use tools::read_file::ReadFileTool;
pub use tools::task::{TaskOutputTool, TaskStopTool};
pub use tools::todo::{TodoReadTool, TodoWriteTool};
pub use tools::write_file::WriteFileTool;

/// Cross-cutting settings the binary feeds into builtin tool
/// construction. Today scopes only image-related flags; new fields
/// will be `Default`-derived so callers can extend without churning
/// every call site.
#[derive(Clone)]
pub struct BuiltinToolOptions {
    /// Forwarded to [`ReadFileTool::with_auto_resize`]. Default
    /// `true`; flip via `image_auto_resize` in `~/.aj/config.toml`.
    pub image_auto_resize: bool,
}

impl Default for BuiltinToolOptions {
    fn default() -> Self {
        Self {
            image_auto_resize: true,
        }
    }
}

/// Build the catalog of every builtin tool, unfiltered.
///
/// Most callers want [`builtin_tools`], which applies the user's
/// disabled-tools set. This raw catalog is for callers that need the
/// full list regardless of config (e.g. argument-name completion).
pub fn get_builtin_tools(options: &BuiltinToolOptions) -> Vec<ErasedToolDefinition> {
    vec![
        AgentTool.into(),
        BashTool.into(),
        ReadFileTool::with_auto_resize(options.image_auto_resize).into(),
        WriteFileTool.into(),
        EditFileTool.into(),
        EditFileMultiTool.into(),
        TaskOutputTool.into(),
        TaskStopTool.into(),
        TodoReadTool.into(),
        TodoWriteTool.into(),
    ]
}

/// Build the builtin tool catalog with the user's disabled tools
/// filtered out, ready for `Agent::with_provider`.
///
/// `disabled` is the `disabled_tools` name set from
/// `~/.aj/config.toml`. Filtering behind this one seam keeps the
/// name-set contract in a single place rather than re-applied at each
/// frontend's call site. The agent never advertises a filtered tool
/// to the model; sub-agents inherit the filtered list (minus the
/// `agent` tool) by cloning.
pub fn builtin_tools(
    options: &BuiltinToolOptions,
    disabled: &[String],
) -> Vec<ErasedToolDefinition> {
    let mut tools = get_builtin_tools(options);
    if !disabled.is_empty() {
        tools.retain(|tool| !disabled.contains(&tool.name));
        tracing::info!(?disabled, "filtered disabled tools");
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_tools_empty_disabled_is_full_catalog() {
        let opts = BuiltinToolOptions::default();
        let all = get_builtin_tools(&opts).len();
        assert_eq!(builtin_tools(&opts, &[]).len(), all);
    }

    #[test]
    fn builtin_tools_drops_disabled_names() {
        let opts = BuiltinToolOptions::default();
        let disabled = vec!["bash".to_string(), "write_file".to_string()];
        let tools = builtin_tools(&opts, &disabled);
        assert!(
            tools.iter().all(|t| !disabled.contains(&t.name)),
            "disabled tools must not appear in the catalog"
        );
        assert_eq!(tools.len(), get_builtin_tools(&opts).len() - disabled.len());
    }

    #[test]
    fn builtin_tools_ignores_unknown_disabled_names() {
        let opts = BuiltinToolOptions::default();
        let tools = builtin_tools(&opts, &["no_such_tool".to_string()]);
        assert_eq!(tools.len(), get_builtin_tools(&opts).len());
    }
}
