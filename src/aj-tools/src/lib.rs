//! Framework and implementations for builtin tools.

use std::path::PathBuf;
use std::time::SystemTime;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub mod tools;
mod util;

pub use tools::glob::GlobTool;
pub use tools::grep::GrepTool;
pub use tools::ls::LsTool;
pub use tools::read_file::ReadFileTool;
use util::derive_schema;

/// A builtin tool that can be used by the agent.
pub trait ToolDefinition {
    /// The input type for this tool.
    type Input: JsonSchema + DeserializeOwned;

    /// The name of the tool.
    fn name(&self) -> &'static str;

    /// A description of the tool, for the language model.
    fn description(&self) -> &'static str;

    /// Execute the tool with the given input.
    fn execute(
        &self,
        session_state: &mut dyn SessionState,
        turn_state: &dyn TurnState,
        input: Self::Input,
    ) -> Result<String, anyhow::Error>;

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
    pub func:
        Box<dyn Fn(&mut dyn SessionState, &dyn TurnState, Value) -> Result<String, anyhow::Error>>,
}

impl<T: ToolDefinition + 'static> From<T> for ErasedToolDefinition {
    fn from(tool: T) -> Self {
        ErasedToolDefinition {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: tool.input_schema(),
            func: Box::new(move |session_state, turn_state, input| {
                let typed_input: T::Input = serde_json::from_value(input)?;
                tool.execute(session_state, turn_state, typed_input)
            }),
        }
    }
}

/// Access to state that is scoped to one agent session or thread.
pub trait SessionState {
    fn working_directory(&self) -> PathBuf;
    fn record_file_access(&mut self, path: PathBuf);
    fn get_file_access_time(&self, path: &PathBuf) -> Option<SystemTime>;
}

/// Access to state that is scoped to one iteration through the agent loop, aka.
/// a turn.
pub trait TurnState {}

pub fn get_builtin_tools() -> Vec<ErasedToolDefinition> {
    vec![
        ReadFileTool.into(),
        LsTool.into(),
        GlobTool.into(),
        GrepTool.into(),
    ]
}
