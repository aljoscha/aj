use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionContext, ToolDefinition, TurnContext};

const DESCRIPTION: &str = r#"
Edit files by doing exact string replacement.

Usage:

- The path parameter must be an absolute path
- The file must exist
- old_string must match exactly one occurrence in the file, you can provide a larger string with more context to make it more unique, or use replace_all to replace all occurences
- If there are zero matches or multiple matches, the operation will fail
- If replace_all is set to true, all occurrences of old_string will be replaced with new_string
"#;

#[derive(Clone)]
pub struct EditFileTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct EditFileInput {
    /// The absolute path to the file to modify.
    path: String,
    /// The exact string to find and replace.
    old_string: String,
    /// The string to replace old_string with.
    new_string: String,
    /// If true, replace all occurrences of old_string. If false or not
    /// provided, replace only if exactly one occurrence exists.
    #[serde(default)]
    replace_all: bool,
}

impl ToolDefinition for EditFileTool {
    type Input = EditFileInput;

    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    async fn execute(
        &self,
        session_ctx: &mut dyn SessionContext,
        _turn_ctx: &mut dyn TurnContext,
        input: Self::Input,
    ) -> Result<String, anyhow::Error> {
        let path = Path::new(&input.path);
        if !path.is_absolute() {
            return Err(anyhow::anyhow!(
                "Path must be absolute, got: {}",
                input.path
            ));
        }

        // Check if file exists
        if !path.exists() {
            return Err(anyhow::anyhow!("File '{}' does not exist", input.path));
        }

        let content = fs::read_to_string(&input.path)
            .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", input.path, e))?;

        let matches: Vec<_> = content.match_indices(&input.old_string).collect();

        if matches.is_empty() {
            return Err(anyhow::anyhow!(
                "No occurrences of '{}' found in file '{}'",
                input.old_string,
                input.path
            ));
        }

        if matches.len() == 1 || input.replace_all {
            let new_content = content.replace(&input.old_string, &input.new_string);

            let display_path = Path::new(path)
                .strip_prefix(session_ctx.working_directory())
                .unwrap_or(Path::new(path))
                .display()
                .to_string();
            session_ctx.display_tool_result_diff(
                "edit_file",
                &display_path,
                &content,
                &new_content,
            );

            fs::write(&input.path, &new_content)
                .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", input.path, e))?;

            return Ok(format!(
                "Successfully replaced '{}' with '{}' in file '{}'",
                input.old_string, input.new_string, input.path
            ));
        }

        Err(anyhow::anyhow!(
            "Found {} occurrences of '{}' in file '{}'. Exactly one occurrence is required for safe replacement. Set replace_all to true to replace all occurrences.",
            matches.len(),
            input.old_string,
            input.path
        ))
    }
}
