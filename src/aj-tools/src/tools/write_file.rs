use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionContext, ToolDefinition, TurnContext};

const DESCRIPTION: &str = r#"
Write a file to the local file system.

Usage:

- The path parameter must be an absolute path
- This will overwrite an existing file if there is one at the given path!
- Prefer editing existing files over creating new ones - only create new files when explicitly required
- IMPORTANT: Don't use this tool for renaming a file. Prefer to use the bash tool with the mv command.
"#;

#[derive(Clone)]
pub struct WriteFileTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct WriteFileInput {
    /// The absolute path to the file to write.
    path: String,
    /// The content to write to the file.
    content: String,
}

impl ToolDefinition for WriteFileTool {
    type Input = WriteFileInput;

    fn name(&self) -> &'static str {
        "write_file"
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

        let file_exists = path.exists();
        let original_content = if file_exists {
            Some(fs::read_to_string(&input.path).unwrap_or_default())
        } else {
            None
        };

        let display_path = Path::new(path)
            .strip_prefix(session_ctx.working_directory())
            .unwrap_or(Path::new(path))
            .display()
            .to_string();

        // Display the diff or file contents to the user
        if let Some(original) = &original_content {
            session_ctx.display_tool_result_diff(
                "write_file",
                &display_path,
                original,
                &input.content,
            );
        } else {
            // New file - display the content
            session_ctx.display_tool_result("write_file", &display_path, &input.content);
        }

        fs::write(&input.path, &input.content)
            .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", input.path, e))?;

        let action = if file_exists { "overwrote" } else { "created" };
        Ok(format!("Successfully {} file '{}'", action, input.path))
    }
}
