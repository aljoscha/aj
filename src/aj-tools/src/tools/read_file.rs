use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionState, ToolDefinition, TurnState};

pub struct ReadFileTool;

impl ToolDefinition for ReadFileTool {
    type Input = ReadFileInput;

    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file with line numbers. Requires an absolute path. Supports optional offset (line to start from) and limit (number of lines to read) parameters."
    }

    fn execute(
        &self,
        _session_state: &mut dyn SessionState,
        _turn_state: &dyn TurnState,
        input: Self::Input,
    ) -> Result<String, anyhow::Error> {
        let path = Path::new(&input.path);
        if !path.is_absolute() {
            return Err(anyhow::anyhow!(
                "Path must be absolute, got: {}",
                input.path
            ));
        }

        let content = fs::read_to_string(&input.path)
            .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", input.path, e))?;

        let lines: Vec<&str> = content.lines().collect();

        // Calculate start and end indices
        let start_idx = input.offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
        let end_idx = match input.limit {
            Some(limit) => (start_idx + limit).min(lines.len()),
            None => lines.len(),
        };

        // Ensure start_idx is within bounds
        if start_idx >= lines.len() {
            return Ok(String::new());
        }

        // Format lines with line numbers
        let formatted_lines: Vec<String> = lines[start_idx..end_idx]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:5}→{}", start_idx + i + 1, line))
            .collect();

        Ok(formatted_lines.join("\n"))
    }
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct ReadFileInput {
    /// The absolute path to the file to read.
    path: String,
    /// The line number to start reading from (1-indexed). If not provided, starts from the beginning.
    #[serde(default)]
    offset: Option<usize>,
    /// The number of lines to read. If not provided, reads all lines from offset to end.
    #[serde(default)]
    limit: Option<usize>,
}
