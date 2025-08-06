use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionContext, ToolDefinition, ToolResult, TurnContext};
use aj_ui::{AjUiAskPermission, UserOutput};

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
        _permission_handler: &dyn AjUiAskPermission,
        input: Self::Input,
    ) -> Result<ToolResult, anyhow::Error> {
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
            .unwrap_or_else(|_| Path::new(path))
            .display()
            .to_string();

        fs::write(&input.path, &input.content)
            .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", input.path, e))?;

        let action = if file_exists { "overwrote" } else { "created" };
        let return_value = format!("Successfully {} file '{}'", action, input.path);

        // Create user output for display
        let user_output = if let Some(original) = &original_content {
            UserOutput::ToolResultDiff {
                tool_name: "write_file".to_string(),
                input: display_path,
                before: original.clone(),
                after: input.content,
            }
        } else {
            UserOutput::ToolResult {
                tool_name: "write_file".to_string(),
                input: display_path,
                output: input.content,
            }
        };

        Ok(ToolResult::with_outputs(return_value, vec![user_output]))
    }
}
