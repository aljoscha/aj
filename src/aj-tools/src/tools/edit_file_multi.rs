use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionState, ToolDefinition, TurnState};

const DESCRIPTION: &str = r#"
Edit files by doing multiple exact string replacements sequentially.

Usage:

- The path parameter must be an absolute path
- The file must exist and have been accessed recently using the read_file tool (access time check prevents changing files that haven't been examined recently enough)
- Each edit's old_string must match exactly one occurrence in the file at the time it's applied, you can provide a larger string with more context to make it more unique, or use replace_all to replace all occurences
- If there are zero matches or multiple matches for any edit, the operation will fail
- If replace_all is set to true for an edit, all occurrences of that edit's old_string will be replaced with new_string
- Edits are applied sequentially, so each subsequent edit works on the state of the file after the previous edit
- All edits are applied atomically, either all succeed or the whole operation fails
- Prefer this tool over edit_file if there are multiple changes to a file that can be batched together in one call to edit_file_multi
"#;

pub struct EditFileMultiTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct EditOperation {
    /// The exact string to find and replace.
    old_string: String,
    /// The string to replace old_string with.
    new_string: String,
    /// If true, replace all occurrences of old_string. If false or not
    /// provided, replace only if exactly one occurrence exists.
    #[serde(default)]
    replace_all: bool,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct EditFileMultiInput {
    /// The absolute path to the file to modify.
    path: String,
    /// Array of edit operations to apply sequentially.
    edits: Vec<EditOperation>,
}

impl ToolDefinition for EditFileMultiTool {
    type Input = EditFileMultiInput;

    fn name(&self) -> &'static str {
        "edit_file_multi"
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

        // Check if file exists
        if !path.exists() {
            return Err(anyhow::anyhow!("File '{}' does not exist", input.path));
        }

        // Check if we've accessed the file recently
        let access_time = session_state.get_file_access_time(&path_buf);

        // Yes, we're YOLO'ing this one. We're not doing any file locking or
        // atomic cas operations and whatnot. This is not meant to be airtight.
        // If the user (that is me) messes with files while the agent works on
        // them, tough luck!
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
                        "File '{}' has been modified since last access. Read the file first before modifying it.",
                        input.path
                    ));
                }
            }
            None => {
                return Err(anyhow::anyhow!(
                    "File '{}' has not been accessed in this session. Read the file first before modifying it.",
                    input.path
                ));
            }
        }

        // Get current content from disk
        let original_content = fs::read_to_string(&input.path)
            .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", input.path, e))?;
        let mut content = original_content.clone();

        // Apply each edit sequentially
        let mut edit_results = Vec::new();
        for (i, edit) in input.edits.iter().enumerate() {
            let matches: Vec<_> = content.match_indices(&edit.old_string).collect();

            if matches.is_empty() {
                return Err(anyhow::anyhow!(
                    "Edit #{}: No occurrences of '{}' found in file '{}'",
                    i + 1,
                    edit.old_string,
                    input.path
                ));
            }

            if matches.len() == 1 || edit.replace_all {
                content = content.replace(&edit.old_string, &edit.new_string);
                edit_results.push(format!(
                    "Edit #{}: replaced '{}' with '{}'",
                    i + 1,
                    edit.old_string,
                    edit.new_string
                ));
            } else {
                return Err(anyhow::anyhow!(
                    "Edit #{}: Found {} occurrences of '{}' in file '{}'. Exactly one occurrence is required for safe replacement. Set replace_all to true to replace all occurrences.",
                    i + 1,
                    matches.len(),
                    edit.old_string,
                    input.path
                ));
            }
        }

        let display_path = Path::new(path)
            .strip_prefix(session_state.working_directory())
            .unwrap_or(Path::new(path))
            .display()
            .to_string();
        session_state.display_tool_result_diff(
            "edit_file_multi",
            &display_path,
            &original_content,
            &content,
        );

        // Write the modified content back to disk
        fs::write(&input.path, &content)
            .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", input.path, e))?;

        Ok(format!(
            "Successfully applied {} edits to file '{}':\n{}",
            input.edits.len(),
            input.path,
            edit_results.join("\n")
        ))
    }
}
