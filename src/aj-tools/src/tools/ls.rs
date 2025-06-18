use globset::{Glob, GlobSetBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::{SessionState, ToolDefinition, TurnState};

const DESCRIPTION: &str = r#"
List entries (files and directories) in a given directory path.

Usage:

- The path parameter must be an absolute path
- Optional ignore parameter accepts an array of glob patterns to exclude from results
- Returns a list of entries with their type (file/directory) and size
- Entries are sorted alphabetically
- You should prefer the glob tool instead if you need recursive search or pattern matching
"#;

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

    fn execute(
        &self,
        _session_state: &mut dyn SessionState,
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

            results.push(format!("{:<20} {:<10} {}", file_name, entry_type, size));
        }

        // Sort results alphabetically
        results.sort();

        if results.is_empty() {
            Ok("No entries found.".to_string())
        } else {
            Ok(format!(
                "{:<20} {:<10} {}\n{}",
                "Name",
                "Type",
                "Size",
                results.join("\n")
            ))
        }
    }
}
