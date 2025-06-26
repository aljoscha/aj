use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionState, ToolDefinition, TurnState};

const DESCRIPTION: &str = r#"
Read the contents of a file from the local file system. If a file does not exist
an error will be returned.

Usage:

- The path parameter must be an absolute path
- Results include line numbers, starting at 1
- You can specify an offset and a limit but it's usually better to read the
  whole file. Use this for reading very big files.
"#;

pub struct ReadFileTool;

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

impl ToolDefinition for ReadFileTool {
    type Input = ReadFileInput;

    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn execute(
        &self,
        session_state: &mut dyn SessionState,
        _turn_state: &mut dyn TurnState,
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

        // Display the file contents to the user
        let selected_content = &lines[start_idx..end_idx];

        let mut display_path = Path::new(path)
            .strip_prefix(session_state.working_directory())
            .unwrap_or(Path::new(path))
            .display()
            .to_string();

        // Append offset and limit information to display path
        if input.offset.is_some() || input.limit.is_some() {
            let start_line = start_idx + 1; // Convert to 1-based line number
            let end_line = end_idx; // Already 1-based for display
            display_path.push_str(&format!(" {}:{}", start_line, end_line));
        }

        let formatted_for_display = format_for_display(selected_content);
        session_state.display_tool_result("read_file", &display_path, &formatted_for_display);

        // Format lines with line numbers for the tool result
        let formatted_lines: Vec<String> = lines[start_idx..end_idx]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:5>}: {}", start_idx + i + 1, line))
            .collect();

        Ok(formatted_lines.join("\n"))
    }
}

/// Formats `read_file` results for display to the user by adding line numbers.
pub fn format_for_display(lines: &[&str]) -> String {
    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        result.push_str(&format!("{:5>}: {}\n", i + 1, line));
    }
    result
}
