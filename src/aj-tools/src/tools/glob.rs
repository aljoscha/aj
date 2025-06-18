use chrono::DateTime;
use globset::{Glob, GlobSetBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::SystemTime;
use walkdir::WalkDir;

use crate::{SessionState, ToolDefinition, TurnState};

const DESCRIPTION: &str = r#"
Recursively find files and directories matching a glob pattern.

Usage:

- The path parameter must be an absolute path to start the search from
- The pattern parameter is a glob pattern to match against relative paths
- Returns a list of matching entries with their absolute paths, type, size, and modification time
- Entries are sorted by modification time (most recent first)
- Use this tool instead of ls when you need to search recursively or match patterns
"#;

pub struct GlobTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct GlobInput {
    /// The absolute path to start the recursive search from.
    pub path: String,
    /// The glob pattern to match against relative paths from the starting directory.
    pub pattern: String,
}

impl ToolDefinition for GlobTool {
    type Input = GlobInput;

    fn name(&self) -> &'static str {
        "glob"
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

        // Build glob pattern
        let glob = Glob::new(&input.pattern)
            .map_err(|e| anyhow::anyhow!("Invalid glob pattern '{}': {}", input.pattern, e))?;
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        let glob_set = builder
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build glob set: {}", e))?;

        #[derive(Debug)]
        struct GlobResult {
            path: String,
            entry_type: String,
            size: String,
            modified: SystemTime,
            modified_str: String,
        }

        let mut results = Vec::new();

        // Walk directory recursively
        for entry in WalkDir::new(&input.path) {
            let entry = entry.map_err(|e| anyhow::anyhow!("Failed to walk directory: {}", e))?;

            // Skip hidden directories (starting with '.')
            if entry.file_type().is_dir() {
                if let Some(file_name) = entry.path().file_name() {
                    if let Some(name_str) = file_name.to_str() {
                        // Don't exclude if this is the root directory, by
                        // checking for depth.
                        if name_str.starts_with('.') && entry.depth() > 0 {
                            continue;
                        }
                    }
                }
            }

            // Get relative path from the starting directory
            let relative_path = entry
                .path()
                .strip_prefix(&input.path)
                .map_err(|e| anyhow::anyhow!("Failed to get relative path: {}", e))?;

            // Skip the root directory itself
            if relative_path.as_os_str().is_empty() {
                continue;
            }

            // Check if the relative path matches our glob pattern
            if glob_set.is_match(relative_path) {
                let absolute_path = entry.path().to_string_lossy().to_string();

                // Get metadata
                let metadata = entry.metadata().map_err(|e| {
                    anyhow::anyhow!("Failed to read metadata for '{}': {}", absolute_path, e)
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

                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let modified_str = match modified.duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(duration) => {
                        let secs = duration.as_secs();
                        let datetime = DateTime::from_timestamp(secs as i64, 0).unwrap_or_default();
                        datetime.format("%Y-%m-%d %H:%M:%S").to_string()
                    }
                    Err(_) => "unknown".to_string(),
                };

                results.push(GlobResult {
                    path: absolute_path,
                    entry_type: entry_type.to_string(),
                    size,
                    modified,
                    modified_str,
                });
            }
        }

        // Sort results by modification time (most recent first)
        results.sort_by(|a, b| b.modified.cmp(&a.modified));

        if results.is_empty() {
            Ok(format!(
                "No entries matching pattern '{}' found.",
                input.pattern
            ))
        } else {
            let formatted_results: Vec<String> = results
                .iter()
                .map(|r| {
                    format!(
                        "{:<10} {:<15} {:<20} {}",
                        r.entry_type, r.size, r.modified_str, r.path
                    )
                })
                .collect();

            Ok(format!(
                "{:<10} {:<15} {:<20} {}\n{}",
                "Type",
                "Size",
                "Modified",
                "Path",
                formatted_results.join("\n")
            ))
        }
    }
}
