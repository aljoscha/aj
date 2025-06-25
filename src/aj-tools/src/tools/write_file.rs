use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionState, ToolDefinition, TurnState};

const DESCRIPTION: &str = r#"
Write a file to the local file system.
ones.

Usage:

- The path parameter must be an absolute path
- This will overwrite an existing file if there is one at the given path!
- For existing files, the agent must have read the file recently (access time check prevents overwriting files that haven't been examined)
- Prefer editing existing files over creating new ones - only create new files when explicitly required
- The tool will fail if attempting to overwrite a file that hasn't been accessed recently enough
- IMPORTANT: Don't use this tool for renaming a file. Prefer to use the bash tool with the mv command.
"#;

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

        let path_buf = path.to_path_buf();

        let file_exists = path.exists();
        let original_content = if file_exists {
            Some(fs::read_to_string(&input.path).unwrap_or_default())
        } else {
            None
        };

        if file_exists {
            // File exists - check if we've accessed it recently
            let access_time = session_state.get_file_access_time(&path_buf);

            match access_time {
                Some(access_time) => {
                    // Get file modification time
                    let metadata = fs::metadata(path).map_err(|e| {
                        anyhow::anyhow!("Failed to get file metadata for '{}': {}", input.path, e)
                    })?;
                    let modified_time = metadata.modified().map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to get modification time for '{}': {}",
                            input.path,
                            e
                        )
                    })?;

                    // Check if we accessed the file after it was last modified
                    if access_time < modified_time {
                        return Err(anyhow::anyhow!(
                            "File '{}' has been modified since last access. Please read the file first before overwriting it.",
                            input.path
                        ));
                    }
                }
                None => {
                    return Err(anyhow::anyhow!(
                        "File '{}' exists but has not been accessed in this session. Please read the file first before overwriting it.",
                        input.path
                    ));
                }
            }
        }

        let display_path = Path::new(path)
            .strip_prefix(session_state.working_directory())
            .unwrap_or(Path::new(path))
            .display()
            .to_string();

        // Display the diff or file contents to the user
        if let Some(original) = &original_content {
            session_state.display_tool_result_diff(
                "write_file",
                &display_path,
                original,
                &input.content,
            );
        } else {
            // New file - display the content
            session_state.display_tool_result("write_file", &display_path, &input.content);
        }

        fs::write(&input.path, &input.content)
            .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", input.path, e))?;

        let action = if file_exists { "overwrote" } else { "created" };
        Ok(format!("Successfully {} file '{}'", action, input.path))
    }
}
