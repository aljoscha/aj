use globset::{Glob, GlobSetBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionContext, ToolDefinition, TurnContext};

const DESCRIPTION: &str = r#"
List entries (files and directories) in a given directory path.

Usage:

- The path parameter must be an absolute path
- Optional ignore parameter accepts an array of glob patterns to exclude from results
- Returns a list of entries with their type (file/directory) and size
- Entries are sorted alphabetically
- You should prefer the glob tool instead if you need recursive search or pattern matching
"#;

#[derive(Clone)]
pub struct LsTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct LsInput {
    /// The absolute path to the directory to list.
    path: String,
    /// Optional array of glob patterns to ignore. Files/directories matching these patterns will be excluded.
    #[serde(default)]
    ignore: Option<Vec<String>>,
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

        let entries = fs::read_dir(&input.path)
            .map_err(|e| anyhow::anyhow!("Failed to read directory '{}': {}", input.path, e))?;

        let mut results = Vec::new();

        for entry in entries {
            let entry =
                entry.map_err(|e| anyhow::anyhow!("Failed to read directory entry: {}", e))?;

            let file_name = entry.file_name().to_string_lossy().to_string();

            // Check if entry should be ignored
            if let Some(ref glob_set) = glob_set {
                if glob_set.is_match(&file_name) {
                    continue;
                }
            }

            let metadata = entry.metadata().map_err(|e| {
                anyhow::anyhow!("Failed to read metadata for '{}': {}", file_name, e)
            })?;

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

            results.push(format!("{file_name:<20} {entry_type:<10} {size}"));
        }

        // Sort results alphabetically
        results.sort();

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

        // Display to user
        let display_input = match input.ignore.as_ref() {
            Some(ignore_patterns) => format!("path: {}, ignore: {:?}", input.path, ignore_patterns),
            None => format!("path: {}", input.path),
        };
        session_ctx.display_tool_result("ls", &display_input, &output);

        Ok(output)
    }
}
