//! `glob` builtin — recursively finds files matching a glob pattern.
//!
//! Migrated to [`aj_agent::tool::ToolDefinition`] per
//! `docs/aj-next-plan.md` §2.2. Returns a [`ToolOutcome`] with
//! [`ToolDetails::Text`]: the `summary` is the relative search-root
//! path (matching `read_file` / `ls` conventions) followed by a
//! ` pattern=<glob>` marker so a collapsed view captures both the
//! target directory and the search pattern. The `body` is the
//! formatted listing of matched entries (or a "no entries" notice).
//!
//! Recoverable errors (path-not-absolute, missing path, not-a-directory,
//! invalid glob pattern, walker IO failures, metadata failures) come
//! back as `is_error: true` outcomes so the model can correct the call
//! instead of aborting the turn.

use aj_agent::tool::{ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use chrono::DateTime;
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const DESCRIPTION: &str = r#"
Recursively find files and directories matching a glob pattern.

Usage:

- The path parameter must be an absolute path to start the search from
- The pattern parameter is a glob pattern to match against relative paths
- Returns a list of matching entries with their absolute paths, type, size, and modification time
- Entries are sorted by modification time (most recent first)
- Automatically respects .gitignore files and ignores hidden files
- Use this tool instead of ls when you need to search recursively or match patterns
"#;

#[derive(Clone)]
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

        // Build the glob matcher. An invalid pattern surfaces as a
        // recoverable error so the model can correct the request.
        let glob = match Glob::new(&input.pattern) {
            Ok(glob) => glob,
            Err(e) => {
                return Ok(error_outcome(
                    &input,
                    ctx,
                    format!("Invalid glob pattern '{}': {}", input.pattern, e),
                ));
            }
        };
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        let glob_set = match builder.build() {
            Ok(set) => set,
            Err(e) => {
                return Ok(error_outcome(
                    &input,
                    ctx,
                    format!("Failed to build glob set: {e}"),
                ));
            }
        };

        let mut results = Vec::new();
        if let Err(e) = collect_matches(&input.path, &glob_set, &mut results) {
            return Ok(error_outcome(&input, ctx, e.to_string()));
        }

        // Most recent first — matches the historical ordering and
        // matches the documented behavior in the tool description.
        results.sort_by(|a, b| b.modified.cmp(&a.modified));

        let body = if results.is_empty() {
            format!("No entries matching pattern '{}' found.", input.pattern)
        } else {
            let formatted: Vec<String> = results
                .iter()
                .map(|r| {
                    format!(
                        "{:<10} {:<15} {:<20} {}",
                        r.entry_type, r.size, r.modified_str, r.path
                    )
                })
                .collect();

            format!(
                "{:<10} {:<15} {:<20} {}\n{}",
                "Type",
                "Size",
                "Modified",
                "Path",
                formatted.join("\n")
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

/// One row in the glob listing, kept around long enough to sort by
/// modification time before formatting.
#[derive(Debug)]
struct GlobResult {
    path: String,
    entry_type: String,
    size: String,
    modified: SystemTime,
    modified_str: String,
}

/// Walk `root`, push every entry whose path (relative to `root`)
/// matches `glob_set` onto `results`. Errors propagate to the caller
/// as a single error message so the top-level execution can convert
/// them to a recoverable [`ToolOutcome`].
fn collect_matches(
    root: &str,
    glob_set: &globset::GlobSet,
    results: &mut Vec<GlobResult>,
) -> anyhow::Result<()> {
    // The walker respects `.gitignore` and skips hidden entries by
    // default — the same defaults `ls` uses.
    let walker = WalkBuilder::new(root).build();

    for result in walker {
        let entry = result.map_err(|e| anyhow::anyhow!("Failed to walk directory: {}", e))?;

        // Match the user's pattern against the path *relative* to
        // the search root so common patterns like `**/*.rs` or
        // `*.toml` behave intuitively.
        let relative_path = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| anyhow::anyhow!("Failed to get relative path: {}", e))?;

        // Skip the root directory itself; it never makes sense as a
        // match and would otherwise show up under empty patterns.
        if relative_path.as_os_str().is_empty() {
            continue;
        }

        if !glob_set.is_match(relative_path) {
            continue;
        }

        let absolute_path = entry.path().to_string_lossy().to_string();

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
                let datetime = DateTime::from_timestamp(i64::try_from(secs).unwrap_or(i64::MAX), 0)
                    .unwrap_or_default();
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

    Ok(())
}

/// Build the collapsed-view headline for a `glob` result.
///
/// Starts from the search-root path made relative to the working
/// directory (matching `read_file` / `ls` conventions) and appends a
/// ` pattern=<glob>` marker so a glance captures both the target and
/// the search pattern. The marker mirrors `ls`'s `ignore=[...]` style.
fn build_summary(input: &GlobInput, ctx: &dyn ToolContext) -> String {
    let path = Path::new(&input.path);
    let mut summary = display_relative(path, &ctx.working_directory());
    summary.push_str(&format!(" pattern={}", input.pattern));
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
fn error_outcome(input: &GlobInput, ctx: &dyn ToolContext, error: String) -> ToolOutcome {
    // Fall back to the raw path for the summary when the path isn't
    // usable (e.g. relative); otherwise mirror the success path's
    // relative-display behavior.
    let summary = if Path::new(&input.path).is_absolute() {
        build_summary(input, ctx)
    } else {
        format!(
            "{} pattern={}",
            PathBuf::from(&input.path).display(),
            input.pattern
        )
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

    /// Matches files at the search root by extension. The wire content
    /// and structured body both list the matches; the summary echoes
    /// the path + pattern.
    #[tokio::test]
    async fn execute_matches_files_by_extension() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("a.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("b.rs"), "fn other() {}").unwrap();
        fs::write(dir.path().join("README.md"), "docs").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = GlobTool
            .execute(
                &mut ctx,
                GlobInput {
                    path: dir.path().display().to_string(),
                    pattern: "*.rs".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("a.rs"), "wire: {wire:?}");
        assert!(wire.contains("b.rs"), "wire: {wire:?}");
        assert!(
            !wire.contains("README.md"),
            "non-matching entry should not appear: {wire:?}"
        );

        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                assert!(
                    summary.contains("pattern=*.rs"),
                    "summary should advertise the pattern: {summary:?}"
                );
                assert!(body.contains("a.rs"), "body: {body:?}");
                assert!(body.contains("b.rs"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Recursive `**/` patterns descend into nested directories.
    #[tokio::test]
    async fn execute_matches_recursive_pattern() {
        let dir = tempdir().expect("temp dir");
        let nested = dir.path().join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("deep.toml"), "[x]").unwrap();
        fs::write(dir.path().join("top.toml"), "[y]").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = GlobTool
            .execute(
                &mut ctx,
                GlobInput {
                    path: dir.path().display().to_string(),
                    pattern: "**/*.toml".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.contains("deep.toml"), "body: {body:?}");
                assert!(body.contains("top.toml"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// No matches still produces a successful outcome with a "no
    /// entries" body so the model gets a clear signal.
    #[tokio::test]
    async fn execute_no_matches_returns_empty_listing() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("alpha.txt"), "hi").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = GlobTool
            .execute(
                &mut ctx,
                GlobInput {
                    path: dir.path().display().to_string(),
                    pattern: "*.rs".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(
                    body.contains("No entries matching pattern '*.rs'"),
                    "body: {body:?}"
                );
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Non-absolute paths surface as a recoverable error outcome.
    #[tokio::test]
    async fn relative_path_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = GlobTool
            .execute(
                &mut ctx,
                GlobInput {
                    path: "relative/dir".to_string(),
                    pattern: "*.rs".to_string(),
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

    /// Missing paths surface as a recoverable error outcome.
    #[tokio::test]
    async fn missing_path_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = GlobTool
            .execute(
                &mut ctx,
                GlobInput {
                    path: "/nonexistent/path/that/should/not/exist".to_string(),
                    pattern: "*.rs".to_string(),
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

    /// Pointing `glob` at a regular file is a recoverable error.
    #[tokio::test]
    async fn file_path_returns_error_outcome() {
        let dir = tempdir().expect("temp dir");
        let file = dir.path().join("regular.txt");
        fs::write(&file, "hi").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = GlobTool
            .execute(
                &mut ctx,
                GlobInput {
                    path: file.display().to_string(),
                    pattern: "*.rs".to_string(),
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

    /// Invalid glob patterns surface as a recoverable error outcome
    /// rather than bubbling up.
    #[tokio::test]
    async fn invalid_glob_pattern_returns_error_outcome() {
        let dir = tempdir().expect("temp dir");
        let mut ctx = DummyToolContext::default();
        let outcome = GlobTool
            .execute(
                &mut ctx,
                GlobInput {
                    path: dir.path().display().to_string(),
                    pattern: "[".to_string(),
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
