use chrono::DateTime;
use grep::regex::RegexMatcher;
use grep::searcher::sinks::UTF8;
use grep::searcher::SearcherBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use std::path::Path;
use std::time::SystemTime;
use walkdir::WalkDir;

use crate::{SessionState, ToolDefinition, TurnState};

const DESCRIPTION: &str = r#"
Search file contents using regular expressions, recursively in a given path.

Usage:

- The path parameter must be an absolute path to start the search from
- The pattern parameter is the regular expression to use for searching
- The include parameter specifies file patterns to include in the search (e.g., "*.rs" or "*.{rs,toml}")
- Returns a list of file paths with at least one match, including file size and modification time
- Results are sorted by modification time (most recent first)
"#;

pub struct GrepTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct GrepInput {
    /// The absolute path to start the recursive search from.
    pub path: String,
    /// File patterns to include in the search (e.g., "*.rs" or "*.{rs,toml}").
    pub include: String,
    /// The regular expression to use for searching.
    pub pattern: String,
}

impl ToolDefinition for GrepTool {
    type Input = GrepInput;

    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn execute(
        &self,
        _session_state: &mut dyn SessionState,
        _turn_state: &dyn TurnState,
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

        let matcher = RegexMatcher::new(&input.pattern)
            .map_err(|e| anyhow::anyhow!("Invalid regex pattern '{}': {}", input.pattern, e))?;

        // Build glob pattern for file filtering
        let glob = globset::Glob::new(&input.include)
            .map_err(|e| anyhow::anyhow!("Invalid include pattern '{}': {}", input.include, e))?;
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(glob);
        let glob_set = builder
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build glob set: {}", e))?;

        #[derive(Debug)]
        struct GrepResult {
            path: String,
            size: String,
            modified: SystemTime,
            modified_str: String,
        }

        let mut results = Vec::new();
        let mut searcher = SearcherBuilder::new().build();

        // Walk directory recursively
        for entry in WalkDir::new(&input.path) {
            let entry = entry.map_err(|e| anyhow::anyhow!("Failed to walk directory: {}", e))?;

            // Only process files
            if !entry.file_type().is_file() {
                continue;
            }

            // Get relative path from the starting directory
            let relative_path = entry
                .path()
                .strip_prefix(&input.path)
                .map_err(|e| anyhow::anyhow!("Failed to get relative path: {}", e))?;

            // Check if the relative path matches our include pattern
            if !glob_set.is_match(relative_path) {
                continue;
            }

            let absolute_path = entry.path().to_string_lossy().to_string();
            let mut has_match = false;

            // Search the file content
            let sink = UTF8(|_lnum, _line| {
                has_match = true;
                Ok(false) // Stop after first match
            });

            if let Err(e) = searcher.search_path(&matcher, entry.path(), sink) {
                // Skip files that can't be read (e.g., binary files, permission issues)
                tracing::debug!("Skipping file '{}': {}", absolute_path, e);
                continue;
            }

            if has_match {
                // Get metadata
                let metadata = entry.metadata().map_err(|e| {
                    anyhow::anyhow!("Failed to read metadata for '{}': {}", absolute_path, e)
                })?;

                let size = format!("{} bytes", metadata.len());
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let modified_str = match modified.duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(duration) => {
                        let secs = duration.as_secs();
                        let datetime = DateTime::from_timestamp(secs as i64, 0).unwrap_or_default();
                        datetime.format("%Y-%m-%d %H:%M:%S").to_string()
                    }
                    Err(_) => "unknown".to_string(),
                };

                results.push(GrepResult {
                    path: absolute_path,
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
                "No files matching pattern '{}' with include filter '{}' found.",
                input.pattern, input.include
            ))
        } else {
            let formatted_results: Vec<String> = results
                .iter()
                .map(|r| {
                    format!(
                        "{:<15} {:<20} {}",
                        r.size, r.modified_str, r.path
                    )
                })
                .collect();

            Ok(format!(
                "{:<15} {:<20} {}\n{}",
                "Size",
                "Modified",
                "Path",
                formatted_results.join("\n")
            ))
        }
    }
}
