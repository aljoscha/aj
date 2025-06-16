use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;

use crate::{SessionState, ToolDefinition, TurnState};

pub struct ReadFileTool;

impl ToolDefinition for ReadFileTool {
    type Input = ReadFileInput;

    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file from the working directory"
    }

    fn execute(
        &self,
        _session_state: &mut dyn SessionState,
        _turn_state: &dyn TurnState,
        input: Self::Input,
    ) -> Result<String, anyhow::Error> {
        let content = fs::read_to_string(&input.path)
            .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", input.path, e))?;

        Ok(content)
    }
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct ReadFileInput {
    /// The relative path of a file in the working directory.
    path: String,
}
