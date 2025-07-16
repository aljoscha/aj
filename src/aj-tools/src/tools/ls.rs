use globset::{Glob, GlobSetBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionContext, ToolDefinition, ToolResult, TurnContext};
use aj_ui::{AjUiAskPermission, UserOutput};

const DESCRIPTION: &str = r#"
List entries (files and directories) in a given directory path.

Usage:

- The path parameter must be an absolute path
- Optional ignore parameter accepts an array of glob patterns to exclude from results
- Optional recursive parameter enables recursive directory traversal with indentation
- Returns a list of entries with their type (file/directory) and size
- Entries are sorted alphabetically
- You should prefer the glob or grep tool if you know roughly what you're looking for and can use pattern matching
"#;

#[derive(Clone)]
pub struct LsTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct LsInput {
    /// The absolute path to the directory to list.
    pub path: String,
    /// Optional array of glob patterns to ignore. Files/directories matching these patterns will be excluded.
    #[serde(default)]
    pub ignore: Option<Vec<String>>,
    /// Optional flag to enable recursive directory traversal with indentation.
    #[serde(default)]
    pub recursive: Option<bool>,
}

impl ToolDefinition for LsTool {
    type Input = LsInput;

    fn name(&self) -> &'static str {
        "ls"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    async fn execute(
        &self,
        _session_ctx: &mut dyn SessionContext,
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

        if !path.exists() {
            return Err(anyhow::anyhow!("Path does not exist: {}", input.path));
        }

        if !path.is_dir() {
            return Err(anyhow::anyhow!("Path is not a directory: {}", input.path));
        }

        // Build glob set for ignore patterns
        let glob_set = if let Some(ignore_patterns) = &input.ignore {
            let mut builder = GlobSetBuilder::new();
            for pattern in ignore_patterns {
                let glob = Glob::new(pattern)
                    .map_err(|e| anyhow::anyhow!("Invalid glob pattern '{}': {}", pattern, e))?;
                builder.add(glob);
            }
            Some(
                builder
                    .build()
                    .map_err(|e| anyhow::anyhow!("Failed to build glob set: {}", e))?,
            )
        } else {
            None
        };

        let is_recursive = input.recursive.unwrap_or(false);
        let mut results = Vec::new();

        if is_recursive {
            list_directory_recursive(&input.path, &glob_set, 0, &mut results)?;
        } else {
            list_directory(&input.path, &glob_set, &mut results)?;
        }

        let output = if results.is_empty() {
            "No entries found.".to_string()
        } else {
            format!(
                "{:<20} {:<10} {}\n{}",
                "Name",
                "Type",
                "Size",
                results.join("\n")
            )
        };

        // Create display input
        let display_input = match (&input.ignore, input.recursive) {
            (Some(ignore_patterns), Some(true)) => format!(
                "path: {}, ignore: {:?}, recursive: true",
                input.path, ignore_patterns
            ),
            (Some(ignore_patterns), _) => {
                format!("path: {}, ignore: {:?}", input.path, ignore_patterns)
            }
            (None, Some(true)) => format!("path: {}, recursive: true", input.path),
            (None, _) => format!("path: {}", input.path),
        };

        let user_output = UserOutput::ToolResult {
            tool_name: "ls".to_string(),
            input: display_input,
            output: output.clone(),
        };

        Ok(ToolResult::with_outputs(output, vec![user_output]))
    }
}

fn list_directory(
    path: &str,
    glob_set: &Option<globset::GlobSet>,
    results: &mut Vec<String>,
) -> Result<(), anyhow::Error> {
    let entries = fs::read_dir(path)
        .map_err(|e| anyhow::anyhow!("Failed to read directory '{}': {}", path, e))?;

    let mut dir_entries = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| anyhow::anyhow!("Failed to read directory entry: {}", e))?;

        let file_name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files and directories (starting with '.')
        if file_name.starts_with('.') {
            continue;
        }

        // Check if entry should be ignored
        if let Some(glob_set) = glob_set {
            if glob_set.is_match(&file_name) {
                continue;
            }
        }

        let metadata = entry
            .metadata()
            .map_err(|e| anyhow::anyhow!("Failed to read metadata for '{}': {}", file_name, e))?;

        let entry_type = if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };

        let size = if metadata.is_file() {
            format!("{} bytes", metadata.len())
        } else {
            "-".to_string()
        };

        dir_entries.push(format!("{file_name:<20} {entry_type:<10} {size}"));
    }

    // Sort results alphabetically
    dir_entries.sort();
    results.extend(dir_entries);

    Ok(())
}

fn list_directory_recursive(
    path: &str,
    glob_set: &Option<globset::GlobSet>,
    depth: usize,
    results: &mut Vec<String>,
) -> Result<(), anyhow::Error> {
    let entries = fs::read_dir(path)
        .map_err(|e| anyhow::anyhow!("Failed to read directory '{}': {}", path, e))?;

    let mut dir_entries = Vec::new();
    let mut subdirs = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| anyhow::anyhow!("Failed to read directory entry: {}", e))?;

        let file_name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files and directories (starting with '.')
        if file_name.starts_with('.') {
            continue;
        }

        // Check if entry should be ignored
        if let Some(glob_set) = glob_set {
            if glob_set.is_match(&file_name) {
                continue;
            }
        }

        let metadata = entry
            .metadata()
            .map_err(|e| anyhow::anyhow!("Failed to read metadata for '{}': {}", file_name, e))?;

        let entry_type = if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };

        let size = if metadata.is_file() {
            format!("{} bytes", metadata.len())
        } else {
            "-".to_string()
        };

        let indent = "  ".repeat(depth);
        let formatted_entry = format!("{indent}{file_name:<20} {entry_type:<10} {size}");
        dir_entries.push(formatted_entry);

        if metadata.is_dir() {
            subdirs.push(entry.path());
        }
    }

    // Sort results alphabetically
    dir_entries.sort();
    results.extend(dir_entries);

    // Recursively process subdirectories
    subdirs.sort();
    for subdir in subdirs {
        if let Some(subdir_str) = subdir.to_str() {
            list_directory_recursive(subdir_str, glob_set, depth + 1, results)?;
        }
    }

    Ok(())
}
