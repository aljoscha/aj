//! `edit_file` builtin — exact-string replacement on a single file.
//!
//! Migrated to [`aj_agent::tool::ToolDefinition`] per
//! `docs/aj-next-plan.md` §2.2. Returns a [`ToolOutcome`] whose
//! `details` is [`ToolDetails::Diff`] on success: `before` is the
//! file's prior content, `after` is the post-replacement content. The
//! wire `content` is the short success summary the legacy
//! implementation emitted so the model still sees a deterministic
//! `"Successfully replaced ..."` line.
//!
//! Recoverable errors (path-not-absolute, file-not-found, read /
//! write failure, zero or ambiguous matches) come back as
//! `is_error: true` outcomes carrying [`ToolDetails::Text`] so the
//! model can correct its call instead of aborting the turn. Per
//! `docs/aj-next-plan.md` §1.3, [`execution_mode`] is overridden to
//! [`ExecutionMode::Sequential`] because this tool mutates the
//! filesystem — the agent will serialize a batch containing it to
//! avoid interleaved writes.
//!
//! [`execution_mode`]: ToolDefinition::execution_mode

use aj_agent::tool::{ExecutionMode, ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DESCRIPTION: &str = r#"
Edit files by doing exact string replacement.

Usage:

- The path parameter must be an absolute path
- The file must exist
- old_string must match exactly one occurrence in the file, you can provide a larger string with more context to make it more unique, or use replace_all to replace all occurences
- If there are zero matches or multiple matches, the operation will fail
- If replace_all is set to true, all occurrences of old_string will be replaced with new_string
"#;

#[derive(Clone)]
pub struct EditFileTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct EditFileInput {
    /// The absolute path to the file to modify.
    pub path: String,
    /// The exact string to find and replace.
    pub old_string: String,
    /// The string to replace old_string with.
    pub new_string: String,
    /// If true, replace all occurrences of old_string. If false or not
    /// provided, replace only if exactly one occurrence exists.
    #[serde(default)]
    pub replace_all: bool,
}

impl ToolDefinition for EditFileTool {
    type Input = EditFileInput;

    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    /// `edit_file` mutates the filesystem; the spec marks it as
    /// `Sequential` so a batch containing it serializes around any
    /// other in-flight tool calls (`docs/aj-next-plan.md` §1.3).
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> anyhow::Result<ToolOutcome> {
        let path = Path::new(&input.path);
        if !path.is_absolute() {
            return Ok(error_outcome(
                &input.path,
                format!("Path must be absolute, got: {}", input.path),
            ));
        }

        if !path.exists() {
            return Ok(error_outcome(
                &input.path,
                format!("File '{}' does not exist", input.path),
            ));
        }

        let original_content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) => {
                return Ok(error_outcome(
                    &input.path,
                    format!("Failed to read file '{}': {}", input.path, e),
                ));
            }
        };

        // Count matches to enforce the "exactly one occurrence unless
        // replace_all" contract before touching the disk. `match_indices`
        // is non-overlapping which matches the legacy behavior and the
        // tool description.
        let match_count = original_content.matches(&input.old_string).count();

        if match_count == 0 {
            return Ok(error_outcome(
                &input.path,
                format!(
                    "No occurrences of '{}' found in file '{}'",
                    input.old_string, input.path
                ),
            ));
        }

        if match_count > 1 && !input.replace_all {
            return Ok(error_outcome(
                &input.path,
                format!(
                    "Found {} occurrences of '{}' in file '{}'. Exactly one occurrence is required for safe replacement. Set replace_all to true to replace all occurrences.",
                    match_count, input.old_string, input.path
                ),
            ));
        }

        let new_content = original_content.replace(&input.old_string, &input.new_string);

        let display_path = display_relative(path, &ctx.working_directory());

        if let Err(e) = fs::write(path, &new_content) {
            return Ok(error_outcome(
                &input.path,
                format!("Failed to write file '{}': {}", input.path, e),
            ));
        }

        let return_value = format!(
            "Successfully replaced '{}' with '{}' in file '{}'",
            input.old_string, input.new_string, input.path
        );

        Ok(ToolOutcome {
            content: vec![UserContent::text(return_value)],
            details: ToolDetails::Diff {
                path: display_path,
                before: original_content,
                after: new_content,
            },
            is_error: false,
        })
    }
}

/// Resolve `path` against `cwd` for display, falling back to the raw
/// path when stripping fails (e.g. the file lives outside the cwd).
fn display_relative(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// Build a [`ToolOutcome`] for a recoverable error. The model gets the
/// human-readable error string as the tool result and `is_error: true`
/// so it can correct the call; the user sees the same string in the
/// CLI's error rendering via the bridge. The summary falls back to the
/// raw path so even non-absolute or otherwise-unusable paths surface
/// something meaningful in collapsed views.
fn error_outcome(path: &str, error: String) -> ToolOutcome {
    ToolOutcome {
        content: vec![UserContent::text(error.clone())],
        details: ToolDetails::Text {
            summary: PathBuf::from(path).display().to_string(),
            body: error,
        },
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;
    use std::io::Write;
    use tempfile::NamedTempFile;

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

    /// Single-occurrence replacement is the common case. The wire
    /// content reports the success summary; the structured `Diff`
    /// payload carries the file's prior content as `before` and the
    /// post-replacement content as `after`.
    #[tokio::test]
    async fn single_occurrence_replacement_returns_diff_outcome() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "alpha beta gamma\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: path.display().to_string(),
                    old_string: "beta".to_string(),
                    new_string: "BETA".to_string(),
                    replace_all: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.starts_with("Successfully replaced"), "wire: {wire:?}");
        assert!(wire.contains("beta"), "wire: {wire:?}");
        assert!(wire.contains("BETA"), "wire: {wire:?}");

        match &outcome.details {
            ToolDetails::Diff {
                path: _,
                before,
                after,
            } => {
                assert_eq!(before, "alpha beta gamma\n");
                assert_eq!(after, "alpha BETA gamma\n");
            }
            other => panic!("expected Diff details, got {other:?}"),
        }

        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "alpha BETA gamma\n");
    }

    /// `replace_all: true` replaces every occurrence in a single
    /// invocation, even when the count is greater than one.
    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "foo foo foo\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: path.display().to_string(),
                    old_string: "foo".to_string(),
                    new_string: "bar".to_string(),
                    replace_all: true,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Diff { before, after, .. } => {
                assert_eq!(before, "foo foo foo\n");
                assert_eq!(after, "bar bar bar\n");
            }
            other => panic!("expected Diff details, got {other:?}"),
        }

        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "bar bar bar\n");
    }

    /// Non-absolute paths surface as a recoverable error outcome
    /// rather than a hard `Err`, so the model can correct its call.
    #[tokio::test]
    async fn relative_path_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: "relative/file.txt".to_string(),
                    old_string: "x".to_string(),
                    new_string: "y".to_string(),
                    replace_all: false,
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

    /// A missing file surfaces as a recoverable error outcome rather
    /// than bubbling an `Err`.
    #[tokio::test]
    async fn missing_file_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: "/nonexistent/path/that/should/not/exist.txt".to_string(),
                    old_string: "x".to_string(),
                    new_string: "y".to_string(),
                    replace_all: false,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.contains("does not exist"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Zero matches surface as a recoverable error outcome and leave
    /// the file untouched.
    #[tokio::test]
    async fn no_match_returns_error_outcome_and_leaves_file_unchanged() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "hello world\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: path.display().to_string(),
                    old_string: "nonexistent".to_string(),
                    new_string: "irrelevant".to_string(),
                    replace_all: false,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.contains("No occurrences of"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }

        // File should not have been touched.
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "hello world\n");
    }

    /// Multiple matches without `replace_all` surface as a recoverable
    /// error outcome and leave the file untouched.
    #[tokio::test]
    async fn multiple_matches_without_replace_all_returns_error_outcome() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "foo foo foo\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: path.display().to_string(),
                    old_string: "foo".to_string(),
                    new_string: "bar".to_string(),
                    replace_all: false,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.contains("Found 3 occurrences"), "body: {body:?}");
                assert!(body.contains("Set replace_all to true"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }

        // File should not have been touched.
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "foo foo foo\n");
    }

    /// Locks in `Sequential` execution mode — the agent's batching
    /// logic relies on this to serialize filesystem mutations
    /// (`docs/aj-next-plan.md` §1.3).
    #[test]
    fn execution_mode_is_sequential() {
        assert_eq!(EditFileTool.execution_mode(), ExecutionMode::Sequential);
    }
}
