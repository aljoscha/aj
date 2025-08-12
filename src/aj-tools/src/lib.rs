//! Framework and implementations for builtin tools.

use std::path::PathBuf;

use aj_ui::{AjUi, UserOutput};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::tools::todo::TodoItem;

pub mod testing;
pub mod tools;
mod util;

/// Result of tool execution that includes both the return value and user outputs
pub struct ToolResult {
    /// The return value of the tool (what goes back to the LLM)
    pub return_value: String,
    /// User outputs that should be displayed to the user
    pub user_outputs: Vec<UserOutput>,
}

impl ToolResult {
    /// Create a new ToolResult with just a return value
    pub fn new(return_value: String) -> Self {
        Self {
            return_value,
            user_outputs: Vec::new(),
        }
    }

    /// Create a new ToolResult with return value and user outputs
    pub fn with_outputs(return_value: String, user_outputs: Vec<UserOutput>) -> Self {
        Self {
            return_value,
            user_outputs,
        }
    }
}

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
use util::derive_schema;

/// A builtin tool that can be used by the agent.
pub trait ToolDefinition {
    /// The input type for this tool.
    type Input: JsonSchema + DeserializeOwned + Send;

    /// The name of the tool.
    fn name(&self) -> &'static str;

    /// A description of the tool, for the language model.
    fn description(&self) -> &'static str;

    /// Execute the tool with the given input.
    fn execute(
        &self,
        session_ctx: &mut dyn SessionContext,
        turn_ctx: &mut dyn TurnContext,
        ui: &dyn AjUi,
        input: Self::Input,
    ) -> impl std::future::Future<Output = Result<ToolResult, anyhow::Error>> + Send;

    /// Derive the JSON schema for this tool's input type. Default
    /// implementation uses the derive_schema utility.
    fn input_schema(&self) -> Value {
        derive_schema::<Self::Input>()
    }
}

/// A type-erased tool definition for working with heterogeneous collections of
/// tools.
pub struct ErasedToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub func: ToolFn,
}

type ToolFn = Box<
    dyn for<'a> Fn(
            &'a mut dyn SessionContext,
            &'a mut dyn TurnContext,
            &'a dyn AjUi,
            Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<ToolResult, anyhow::Error>> + Send + 'a>,
        > + Send
        + Sync,
>;

impl<T: ToolDefinition + Send + Sync + Clone + 'static> From<T> for ErasedToolDefinition {
    fn from(tool: T) -> Self {
        ErasedToolDefinition {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: tool.input_schema(),
            func: Box::new(move |session_ctx, turn_ctx, ui, input| {
                let typed_input: T::Input = match serde_json::from_value(input) {
                    Ok(input) => input,
                    Err(e) => return Box::pin(async move { Err(e.into()) }),
                };
                let tool_clone = tool.clone();
                Box::pin(async move {
                    tool_clone
                        .execute(session_ctx, turn_ctx, ui, typed_input)
                        .await
                })
            }),
        }
    }
}

/// Access to state that is scoped to one agent session or thread.
pub trait SessionContext: Send {
    fn working_directory(&self) -> PathBuf;

    /// Get the current todo list for the session.
    fn get_todo_list(&self) -> Vec<TodoItem>;

    /// Set the todo list for the session.
    fn set_todo_list(&mut self, todos: Vec<TodoItem>);

    /// Spawn a sub-agent to perform a specific task.
    ///
    /// The sub-agent will run independently with its own UI wrapper and return a report.
    fn spawn_agent(
        &mut self,
        task: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + '_>,
    >;
}

/// Access to state that is scoped to one iteration through the agent loop, aka.
/// a turn.
pub trait TurnContext: Send {}

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
