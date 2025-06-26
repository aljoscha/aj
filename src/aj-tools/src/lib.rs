//! Framework and implementations for builtin tools.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::tools::todo::TodoItem;

pub mod tools;
mod util;

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
        session_state: &mut dyn SessionState,
        turn_state: &mut dyn TurnState,
        input: Self::Input,
    ) -> impl std::future::Future<Output = Result<String, anyhow::Error>> + Send;

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
            &'a mut dyn SessionState,
            &'a mut dyn TurnState,
            Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + 'a>,
        > + Send
        + Sync,
>;

impl<T: ToolDefinition + Send + Sync + Clone + 'static> From<T> for ErasedToolDefinition {
    fn from(tool: T) -> Self {
        ErasedToolDefinition {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: tool.input_schema(),
            func: Box::new(move |session_state, turn_state, input| {
                let typed_input: T::Input = match serde_json::from_value(input) {
                    Ok(input) => input,
                    Err(e) => return Box::pin(async move { Err(e.into()) }),
                };
                let tool_clone = tool.clone();
                Box::pin(async move {
                    tool_clone
                        .execute(session_state, turn_state, typed_input)
                        .await
                })
            }),
        }
    }
}

/// Access to state that is scoped to one agent session or thread.
pub trait SessionState: Send {
    fn working_directory(&self) -> PathBuf;

    fn display_tool_result(&self, tool_name: &str, input: &str, output: &str);
    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str);
    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str);

    /// Ask the user for permission to perform an action.
    ///
    /// Default implementation returns false (deny permission).
    fn ask_permission(&self, _message: &str) -> bool {
        false
    }

    /// Get the current todo list for the session.
    fn get_todo_list(&self) -> Vec<TodoItem>;

    /// Set the todo list for the session.
    fn set_todo_list(&mut self, todos: Vec<TodoItem>);
}

/// Access to state that is scoped to one iteration through the agent loop, aka.
/// a turn.
pub trait TurnState: Send {}

pub fn get_builtin_tools() -> Vec<ErasedToolDefinition> {
    vec![
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
