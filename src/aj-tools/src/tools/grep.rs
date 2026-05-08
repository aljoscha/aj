//! `grep` builtin — recursively searches file contents using a regex.
//!
//! Migrated to [`aj_agent::tool::ToolDefinition`] per
//! `docs/aj-next-plan.md` §2.2. Returns a [`ToolOutcome`] with
//! [`ToolDetails::Text`]: the `summary` is the relative search-root
//! path (matching `read_file` / `ls` / `glob` conventions) followed
//! by a ` pattern=<regex>` marker and an optional ` include=<glob>`
//! marker so a collapsed view captures both the target and the search
//! parameters. The `body` is the formatted listing of matched files
//! (or a "no matches" notice).
//!
//! Recoverable errors (path-not-absolute, missing path, not-a-directory,
//! invalid regex, invalid include glob, walker IO failures, metadata
//! failures) come back as `is_error: true` outcomes so the model can
//! correct the call instead of aborting the turn. Files that can't be
//! searched (binary files, permission errors) are silently skipped to
//! match the historical behavior.

use aj_agent::tool::{ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use chrono::DateTime;
use grep::regex::RegexMatcher;
use grep::searcher::SearcherBuilder;
use grep::searcher::sinks::UTF8;
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const DESCRIPTION: &str = r#"
Search file contents using regular expressions, recursively in a given path.

Usage:

- The path parameter must be an absolute path to start the search from
- The pattern parameter is the regular expression to use for searching
- The include parameter specifies file patterns to include in the search (e.g., "*.rs" or "*.{rs,toml}"). Defaults to "*" (all files) if not provided.
- Returns a list of file paths with at least one match, including file size and modification time
- Results are sorted by modification time (most recent first)
- Automatically respects .gitignore files and ignores hidden files
"#;

#[derive(Clone)]
pub struct GrepTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct GrepInput {
    /// The absolute path to start the recursive search from.
    pub path: String,
    /// File patterns to include in the search (e.g., "*.rs" or "*.{rs,toml}"). Defaults to "*" if not provided.
    #[serde(default)]
    pub include: Option<String>,
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

        // Compile the user's regex. Invalid patterns surface as
        // recoverable errors so the model can correct the request.
        let matcher = match RegexMatcher::new(&input.pattern) {
            Ok(matcher) => matcher,
            Err(e) => {
                return Ok(error_outcome(
                    &input,
                    ctx,
                    format!("Invalid regex pattern '{}': {}", input.pattern, e),
                ));
            }
        };

        // Build the include-glob filter. Default to "*" so an unset
        // include is equivalent to "every file the walker yields".
        let include_pattern = input.include.as_deref().unwrap_or("*");
        let glob = match globset::Glob::new(include_pattern) {
            Ok(glob) => glob,
            Err(e) => {
                return Ok(error_outcome(
                    &input,
                    ctx,
                    format!("Invalid include pattern '{include_pattern}': {e}"),
                ));
            }
        };
        let mut builder = globset::GlobSetBuilder::new();
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
        if let Err(e) = collect_matches(&input.path, &matcher, &glob_set, &mut results) {
            return Ok(error_outcome(&input, ctx, e.to_string()));
        }

        // Most recent first — matches the historical ordering and the
        // documented behavior in the tool description.
        results.sort_by(|a, b| b.modified.cmp(&a.modified));

        let body = if results.is_empty() {
            format!(
                "No files matching pattern '{}' with include filter '{}' found.",
                input.pattern, include_pattern
            )
        } else {
            let formatted: Vec<String> = results
                .iter()
                .map(|r| format!("{:<15} {:<20} {}", r.size, r.modified_str, r.path))
                .collect();

            format!(
                "{:<15} {:<20} {}\n{}",
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

/// One row in the grep listing, kept around long enough to sort by
/// modification time before formatting.
#[derive(Debug)]
struct GrepResult {
    path: String,
    size: String,
    modified: SystemTime,
    modified_str: String,
}

/// Walk `root`, search every file whose path (relative to `root`)
/// matches `glob_set` for `matcher`, and push entries with at least
/// one match onto `results`. Errors propagate to the caller as a
/// single error message so the top-level execution can convert them
/// to a recoverable [`ToolOutcome`].
fn collect_matches(
    root: &str,
    matcher: &RegexMatcher,
    glob_set: &globset::GlobSet,
    results: &mut Vec<GrepResult>,
) -> anyhow::Result<()> {
    // The walker respects `.gitignore` and skips hidden entries by
    // default — the same defaults `ls` and `glob` use.
    let walker = WalkBuilder::new(root).build();

    let mut searcher = SearcherBuilder::new().build();

    for result in walker {
        let entry = result.map_err(|e| anyhow::anyhow!("Failed to walk directory: {}", e))?;

        // Only search regular files; directories and other entry
        // kinds have no content to match against.
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }

        // Match the include glob against the path *relative* to the
        // search root so common patterns like `*.rs` or `**/*.toml`
        // behave intuitively.
        let relative_path = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| anyhow::anyhow!("Failed to get relative path: {}", e))?;

        if !glob_set.is_match(relative_path) {
            continue;
        }

        let absolute_path = entry.path().to_string_lossy().to_string();
        let mut has_match = false;

        // Stop on the first hit — we only need to know whether the
        // file matched, not which lines.
        let sink = UTF8(|_lnum, _line| {
            has_match = true;
            Ok(false)
        });

        if let Err(e) = searcher.search_path(matcher, entry.path(), sink) {
            // Skip files we can't read (binary content, permission
            // errors, etc.). Surfacing every such failure would drown
            // out actual matches; the historical tool quietly skipped
            // them too.
            tracing::debug!("Skipping file '{}': {}", absolute_path, e);
            continue;
        }

        if !has_match {
            continue;
        }

        let metadata = entry.metadata().map_err(|e| {
            anyhow::anyhow!("Failed to read metadata for '{}': {}", absolute_path, e)
        })?;

        let size = format!("{} bytes", metadata.len());
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

        results.push(GrepResult {
            path: absolute_path,
            size,
            modified,
            modified_str,
        });
    }

    Ok(())
}

/// Build the collapsed-view headline for a `grep` result.
///
/// Starts from the search-root path made relative to the working
/// directory (matching `read_file` / `ls` / `glob` conventions) and
/// appends a ` pattern=<regex>` marker, plus an optional
/// ` include=<glob>` marker when the call narrowed the file set.
/// The marker style mirrors `ls`'s `ignore=[...]` and `glob`'s
/// `pattern=...`.
fn build_summary(input: &GrepInput, ctx: &dyn ToolContext) -> String {
    let path = Path::new(&input.path);
    let mut summary = display_relative(path, &ctx.working_directory());
    summary.push_str(&format!(" pattern={}", input.pattern));
    if let Some(include) = input.include.as_deref() {
        summary.push_str(&format!(" include={include}"));
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
fn error_outcome(input: &GrepInput, ctx: &dyn ToolContext, error: String) -> ToolOutcome {
    // Fall back to the raw path for the summary when the path isn't
    // usable (e.g. relative); otherwise mirror the success path's
    // relative-display behavior.
    let summary = if Path::new(&input.path).is_absolute() {
        build_summary(input, ctx)
    } else {
        let mut summary = format!(
            "{} pattern={}",
            PathBuf::from(&input.path).display(),
            input.pattern
        );
        if let Some(include) = input.include.as_deref() {
            summary.push_str(&format!(" include={include}"));
        }
        summary
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

    /// Files containing the regex appear in the result; files that
    /// don't are excluded. The wire content and the structured body
    /// both list the matches; the summary echoes the path + pattern.
    #[tokio::test]
    async fn execute_finds_files_matching_pattern() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("hit.rs"), "fn target() {}").unwrap();
        fs::write(dir.path().join("miss.rs"), "fn other() {}").unwrap();
        fs::write(dir.path().join("also_hit.txt"), "the target sentence").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = GrepTool
            .execute(
                &mut ctx,
                GrepInput {
                    path: dir.path().display().to_string(),
                    include: None,
                    pattern: "target".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("hit.rs"), "wire: {wire:?}");
        assert!(wire.contains("also_hit.txt"), "wire: {wire:?}");
        assert!(
            !wire.contains("miss.rs"),
            "non-matching file should be excluded: {wire:?}"
        );

        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                assert!(
                    summary.contains("pattern=target"),
                    "summary should advertise the pattern: {summary:?}"
                );
                assert!(
                    !summary.contains("include="),
                    "no include filter → no include marker: {summary:?}"
                );
                assert!(body.contains("hit.rs"), "body: {body:?}");
                assert!(body.contains("also_hit.txt"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Setting `include` narrows the file set without changing the
    /// regex semantics, and the summary picks up an `include=...`
    /// marker so the collapsed view advertises the filter.
    #[tokio::test]
    async fn execute_honors_include_filter() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("a.rs"), "needle here").unwrap();
        fs::write(dir.path().join("a.txt"), "needle here too").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = GrepTool
            .execute(
                &mut ctx,
                GrepInput {
                    path: dir.path().display().to_string(),
                    include: Some("*.rs".to_string()),
                    pattern: "needle".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                assert!(
                    summary.contains("include=*.rs"),
                    "summary should advertise the include filter: {summary:?}"
                );
                assert!(body.contains("a.rs"), "body: {body:?}");
                assert!(
                    !body.contains("a.txt"),
                    "include should exclude a.txt: {body:?}"
                );
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// No matches still produces a successful outcome with a "no
    /// files" body so the model gets a clear signal.
    #[tokio::test]
    async fn execute_no_matches_returns_empty_listing() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("alpha.txt"), "nothing relevant here").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = GrepTool
            .execute(
                &mut ctx,
                GrepInput {
                    path: dir.path().display().to_string(),
                    include: None,
                    pattern: "absent_token".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(
                    body.contains("No files matching pattern 'absent_token'"),
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
        let outcome = GrepTool
            .execute(
                &mut ctx,
                GrepInput {
                    path: "relative/dir".to_string(),
                    include: None,
                    pattern: "x".to_string(),
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
        let outcome = GrepTool
            .execute(
                &mut ctx,
                GrepInput {
                    path: "/nonexistent/path/that/should/not/exist".to_string(),
                    include: None,
                    pattern: "x".to_string(),
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

    /// Pointing `grep` at a regular file is a recoverable error.
    #[tokio::test]
    async fn file_path_returns_error_outcome() {
        let dir = tempdir().expect("temp dir");
        let file = dir.path().join("regular.txt");
        fs::write(&file, "hi").unwrap();

        let mut ctx = DummyToolContext::default();
        let outcome = GrepTool
            .execute(
                &mut ctx,
                GrepInput {
                    path: file.display().to_string(),
                    include: None,
                    pattern: "x".to_string(),
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

    /// Invalid regexes surface as a recoverable error outcome rather
    /// than bubbling up.
    #[tokio::test]
    async fn invalid_regex_returns_error_outcome() {
        let dir = tempdir().expect("temp dir");
        let mut ctx = DummyToolContext::default();
        let outcome = GrepTool
            .execute(
                &mut ctx,
                GrepInput {
                    path: dir.path().display().to_string(),
                    include: None,
                    // Unclosed character class — guaranteed to fail
                    // regex compilation.
                    pattern: "[".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.starts_with("Invalid regex pattern"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Invalid include globs surface as a recoverable error outcome.
    #[tokio::test]
    async fn invalid_include_glob_returns_error_outcome() {
        let dir = tempdir().expect("temp dir");
        let mut ctx = DummyToolContext::default();
        let outcome = GrepTool
            .execute(
                &mut ctx,
                GrepInput {
                    path: dir.path().display().to_string(),
                    include: Some("[".to_string()),
                    pattern: "x".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(
                    body.starts_with("Invalid include pattern"),
                    "body: {body:?}"
                );
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }
}
