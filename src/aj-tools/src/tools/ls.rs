//! `ls` builtin — lists the entries of a directory.
//!
//! Migrated to [`aj_agent::tool::ToolDefinition`] per
//! `docs/aj-next-plan.md` §2.2. Returns a [`ToolOutcome`] with
//! [`ToolDetails::Text`]: the `summary` is the relative directory path
//! (with optional ` (recursive)` and ` ignore=[...]` suffixes
//! describing the request) and the `body` is the formatted entry
//! listing. Recoverable errors (path-not-absolute, missing path,
//! not-a-directory, invalid glob pattern, walker IO failures) come
//! back as `is_error: true` outcomes so the model can correct the
//! call.

use aj_agent::tool::{ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const DESCRIPTION: &str = r#"
List entries (files and directories) in a given directory path.

Usage:

- The path parameter must be an absolute path
- Optional ignore parameter accepts an array of glob patterns to exclude from results
- Optional recursive parameter enables recursive directory traversal with indentation
- Returns a list of entries with their type (file/directory) and size
- Entries are sorted alphabetically
- Automatically respects .gitignore files and ignores hidden files
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
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> anyhow::Result<ToolOutcome> {
        let path = Path::new(&input.path);
        if !path.is_absolute() {
            return Ok(error_outcome(
                &input,
                ctx,
                format!("Path must be absolute, got: {}", input.path),
            ));
        }
        if !path.exists() {
            return Ok(error_outcome(
                &input,
                ctx,
                format!("Path does not exist: {}", input.path),
            ));
        }
        if !path.is_dir() {
            return Ok(error_outcome(
                &input,
                ctx,
                format!("Path is not a directory: {}", input.path),
            ));
        }

        // Build glob set for the user-provided ignore patterns. An
        // invalid pattern is reported as a recoverable error so the
        // model can fix and retry instead of bubbling up an `Err`.
        let glob_set = match &input.ignore {
            Some(patterns) => {
                let mut builder = GlobSetBuilder::new();
                for pattern in patterns {
                    let glob = match Glob::new(pattern) {
                        Ok(glob) => glob,
                        Err(e) => {
                            return Ok(error_outcome(
                                &input,
                                ctx,
                                format!("Invalid glob pattern '{pattern}': {e}"),
                            ));
                        }
                    };
                    builder.add(glob);
                }
                match builder.build() {
                    Ok(set) => Some(set),
                    Err(e) => {
                        return Ok(error_outcome(
                            &input,
                            ctx,
                            format!("Failed to build glob set: {e}"),
                        ));
                    }
                }
            }
            None => None,
        };

        let is_recursive = input.recursive.unwrap_or(false);
        let mut results = Vec::new();
        let listing = if is_recursive {
            list_directory_recursive(&input.path, glob_set.as_ref(), &mut results)
        } else {
            list_directory(&input.path, glob_set.as_ref(), &mut results)
        };
        if let Err(e) = listing {
            return Ok(error_outcome(&input, ctx, e.to_string()));
        }

        let body = if results.is_empty() {
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

        Ok(ToolOutcome {
            content: vec![UserContent::text(body.clone())],
            details: ToolDetails::Text {
                summary: build_summary(&input, ctx),
                body,
            },
            is_error: false,
        })
    }
}

/// Build the collapsed-view headline for an `ls` result.
///
/// Starts from the directory path made relative to the working
/// directory (matching `read_file`'s convention). Appends
/// ` (recursive)` and `ignore=[...]` markers describing the request
/// so a glance at the summary captures both target and mode.
fn build_summary(input: &LsInput, ctx: &dyn ToolContext) -> String {
    let path = Path::new(&input.path);
    let mut summary = display_relative(path, &ctx.working_directory());
    if input.recursive.unwrap_or(false) {
        summary.push_str(" (recursive)");
    }
    if let Some(patterns) = &input.ignore {
        if !patterns.is_empty() {
            summary.push_str(&format!(" ignore={patterns:?}"));
        }
    }
    summary
}

/// Resolve `path` against `cwd` for display, falling back to the raw
/// path when stripping fails (e.g. the directory lives outside the
/// cwd).
fn display_relative(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// Build a [`ToolOutcome`] for a recoverable error. The model gets the
/// human-readable error string as the tool result and `is_error: true`
/// so it can correct the call; the user sees the same string in the
/// CLI's error rendering via the bridge.
fn error_outcome(input: &LsInput, ctx: &dyn ToolContext, error: String) -> ToolOutcome {
    // Fall back to the raw path for the summary when the path isn't
    // usable (e.g. relative or missing); otherwise mirror the success
    // path's relative-display behavior.
    let summary = if Path::new(&input.path).is_absolute() {
        build_summary(input, ctx)
    } else {
        PathBuf::from(&input.path).display().to_string()
    };
    ToolOutcome {
        content: vec![UserContent::text(error.clone())],
        details: ToolDetails::Text {
            summary,
            body: error,
        },
        is_error: true,
    }
}

fn list_directory(
    path: &str,
    glob_set: Option<&globset::GlobSet>,
    results: &mut Vec<String>,
) -> anyhow::Result<()> {
    let mut dir_entries = Vec::new();

    // Use `ignore`'s walker with `max_depth(1)` to list only immediate
    // directory contents. This respects `.gitignore` and skips hidden
    // files by default.
    let walker = WalkBuilder::new(path).max_depth(Some(1)).build();

    for result in walker {
        let entry = result.map_err(|e| anyhow::anyhow!("Failed to walk directory: {}", e))?;

        // Skip the root directory itself.
        if entry.path() == Path::new(path) {
            continue;
        }

        let file_name = entry.file_name().to_str().unwrap_or("").to_string();

        // Drop entries matching any user-supplied ignore pattern.
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

    // Sort results alphabetically for stable output.
    dir_entries.sort();
    results.extend(dir_entries);

    Ok(())
}

fn list_directory_recursive(
    path: &str,
    glob_set: Option<&globset::GlobSet>,
    results: &mut Vec<String>,
) -> anyhow::Result<()> {
    // Walk the entire tree. `.gitignore` is respected and hidden
    // entries are skipped by default.
    let walker = WalkBuilder::new(path).build();

    for result in walker {
        let entry = result.map_err(|e| anyhow::anyhow!("Failed to walk directory: {}", e))?;

        // Skip the root directory itself.
        if entry.path() == Path::new(path) {
            continue;
        }

        let file_name = entry.file_name().to_str().unwrap_or("").to_string();

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

        // Indent two spaces per directory level relative to the root.
        let relative_path = entry
            .path()
            .strip_prefix(path)
            .unwrap_or_else(|_| entry.path());
        let depth = relative_path.components().count().saturating_sub(1);
        let indent = "  ".repeat(depth);

        results.push(format!("{indent}{file_name:<20} {entry_type:<10} {size}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;
    use std::fs;
    use tempfile::tempdir;

    fn extract_text(content: &[UserContent]) -> String {
        content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Lists an immediate directory (no recursion). The entries appear
    /// in the wire content and the structured `Text` body, and the
    /// summary echoes the directory path with no mode markers.
    #[tokio::test]
    async fn execute_lists_directory_non_recursive() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        fs::write(dir.path().join("b.txt"), "beta").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = LsTool
            .execute(
                &mut ctx,
                LsInput {
                    path: dir.path().display().to_string(),
                    ignore: None,
                    recursive: None,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("a.txt"), "wire content: {wire:?}");
        assert!(wire.contains("b.txt"), "wire content: {wire:?}");
        assert!(wire.contains("sub"), "wire content: {wire:?}");

        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                assert!(
                    !summary.contains("(recursive)"),
                    "non-recursive summary should not advertise recursion: {summary:?}"
                );
                assert!(
                    !summary.contains("ignore="),
                    "summary without ignore patterns should not include ignore=: {summary:?}"
                );
                // Header row plus three entries.
                assert!(body.contains("a.txt"), "body: {body:?}");
                assert!(body.contains("b.txt"), "body: {body:?}");
                assert!(body.contains("sub"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Recursive mode descends into subdirectories and the summary
    /// picks up a `(recursive)` marker.
    #[tokio::test]
    async fn execute_recursive_descends_and_advertises_mode() {
        let dir = tempdir().expect("temp dir");
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("nested.txt"), "nested").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = LsTool
            .execute(
                &mut ctx,
                LsInput {
                    path: dir.path().display().to_string(),
                    ignore: None,
                    recursive: Some(true),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                assert!(
                    summary.contains("(recursive)"),
                    "recursive summary should advertise the mode: {summary:?}"
                );
                assert!(body.contains("nested.txt"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// User-provided ignore patterns drop matching entries; the
    /// summary records the patterns so a collapsed view still shows
    /// what was filtered.
    #[tokio::test]
    async fn execute_honors_ignore_patterns() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        fs::write(dir.path().join("drop.tmp"), "drop").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = LsTool
            .execute(
                &mut ctx,
                LsInput {
                    path: dir.path().display().to_string(),
                    ignore: Some(vec!["*.tmp".to_string()]),
                    recursive: None,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                assert!(
                    summary.contains("ignore=[\"*.tmp\"]"),
                    "summary should include ignore patterns: {summary:?}"
                );
                assert!(body.contains("keep.txt"), "body: {body:?}");
                assert!(
                    !body.contains("drop.tmp"),
                    "ignored entry should not appear in body: {body:?}"
                );
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Non-absolute paths surface as a recoverable error outcome
    /// rather than a bubbled `Err`.
    #[tokio::test]
    async fn relative_path_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = LsTool
            .execute(
                &mut ctx,
                LsInput {
                    path: "relative/dir".to_string(),
                    ignore: None,
                    recursive: None,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.starts_with("Path must be absolute"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Missing paths surface as an error outcome with the same
    /// recoverable shape.
    #[tokio::test]
    async fn missing_path_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = LsTool
            .execute(
                &mut ctx,
                LsInput {
                    path: "/nonexistent/path/that/should/not/exist".to_string(),
                    ignore: None,
                    recursive: None,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.starts_with("Path does not exist"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Pointing `ls` at a regular file is a recoverable error so the
    /// model can switch to `read_file`.
    #[tokio::test]
    async fn file_path_returns_error_outcome() {
        let dir = tempdir().expect("temp dir");
        let file = dir.path().join("regular.txt");
        fs::write(&file, "hi").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = LsTool
            .execute(
                &mut ctx,
                LsInput {
                    path: file.display().to_string(),
                    ignore: None,
                    recursive: None,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(
                    body.starts_with("Path is not a directory"),
                    "body: {body:?}"
                );
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Invalid glob patterns surface as a recoverable error so the
    /// model can correct the request.
    #[tokio::test]
    async fn invalid_glob_pattern_returns_error_outcome() {
        let dir = tempdir().expect("temp dir");
        let mut ctx = DummyToolContext::default();
        let outcome = LsTool
            .execute(
                &mut ctx,
                LsInput {
                    path: dir.path().display().to_string(),
                    ignore: Some(vec!["[".to_string()]),
                    recursive: None,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.starts_with("Invalid glob pattern"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }
}
