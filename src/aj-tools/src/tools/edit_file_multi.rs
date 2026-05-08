//! `edit_file_multi` builtin — apply multiple exact-string replacements
//! to a single file, in source order.
//!
//! Migrated to [`aj_agent::tool::ToolDefinition`] per
//! `docs/aj-next-plan.md` §2.2. Returns a [`ToolOutcome`] whose
//! `details` is [`ToolDetails::Diff`] on success: `before` is the
//! file's prior content, `after` is the content after every edit
//! has been applied. The wire `content` keeps the legacy
//! `"Successfully applied N edits ..."` summary so the model still
//! reads a deterministic confirmation.
//!
//! Edits run sequentially against an in-memory copy of the file —
//! each edit's `old_string` is matched against the result of all
//! prior edits. The disk write only happens once every edit has
//! validated, so a failure mid-batch leaves the file untouched
//! (the documented "all edits applied atomically" contract).
//!
//! Recoverable errors (path-not-absolute, file-not-found, read /
//! write failure, zero or ambiguous matches at any step) come back
//! as `is_error: true` outcomes carrying [`ToolDetails::Text`] so
//! the model can correct its call instead of aborting the turn.
//!
//! Per `docs/aj-next-plan.md` §1.3, [`execution_mode`] is overridden
//! to [`ExecutionMode::Sequential`] because this tool mutates the
//! filesystem — the agent serializes a batch containing it to avoid
//! interleaved writes.
//!
//! [`execution_mode`]: ToolDefinition::execution_mode

use aj_agent::tool::{ExecutionMode, ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DESCRIPTION: &str = r#"
Edit files by doing multiple exact string replacements sequentially.

Usage:

- The path parameter must be an absolute path
- The file must exist
- Each edit's old_string must match exactly one occurrence in the file at the time it's applied, you can provide a larger string with more context to make it more unique, or use replace_all to replace all occurences
- If there are zero matches or multiple matches for any edit, the operation will fail
- If replace_all is set to true for an edit, all occurrences of that edit's old_string will be replaced with new_string
- Edits are applied sequentially, so each subsequent edit works on the state of the file after the previous edit
- All edits are applied atomically, either all succeed or the whole operation fails
- Prefer this tool over edit_file if there are multiple changes to a file that can be batched together in one call to edit_file_multi
"#;

#[derive(Clone)]
pub struct EditFileMultiTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct EditOperation {
    /// The exact string to find and replace.
    pub old_string: String,
    /// The string to replace old_string with.
    pub new_string: String,
    /// If true, replace all occurrences of old_string. If false or not
    /// provided, replace only if exactly one occurrence exists.
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct EditFileMultiInput {
    /// The absolute path to the file to modify.
    pub path: String,
    /// Array of edit operations to apply sequentially.
    pub edits: Vec<EditOperation>,
}

impl ToolDefinition for EditFileMultiTool {
    type Input = EditFileMultiInput;

    fn name(&self) -> &'static str {
        "edit_file_multi"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    /// `edit_file_multi` mutates the filesystem; the spec marks it as
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

        // Apply each edit sequentially against the in-memory copy.
        // Disk is not touched until every edit has validated, so any
        // failure mid-batch leaves the file in its original state.
        // `matches(...).count()` is non-overlapping, matching both
        // the prior behavior and the description's "exactly one
        // occurrence" contract.
        let mut content = original_content.clone();
        let mut edit_results = Vec::with_capacity(input.edits.len());
        for (i, edit) in input.edits.iter().enumerate() {
            let match_count = content.matches(&edit.old_string).count();

            if match_count == 0 {
                return Ok(error_outcome(
                    &input.path,
                    format!(
                        "Edit #{}: No occurrences of '{}' found in file '{}'",
                        i + 1,
                        edit.old_string,
                        input.path
                    ),
                ));
            }

            if match_count > 1 && !edit.replace_all {
                return Ok(error_outcome(
                    &input.path,
                    format!(
                        "Edit #{}: Found {} occurrences of '{}' in file '{}'. Exactly one occurrence is required for safe replacement. Set replace_all to true to replace all occurrences.",
                        i + 1,
                        match_count,
                        edit.old_string,
                        input.path
                    ),
                ));
            }

            content = content.replace(&edit.old_string, &edit.new_string);
            edit_results.push(format!(
                "Edit #{}: replaced '{}' with '{}'",
                i + 1,
                edit.old_string,
                edit.new_string
            ));
        }

        let display_path = display_relative(path, &ctx.working_directory());

        if let Err(e) = fs::write(path, &content) {
            return Ok(error_outcome(
                &input.path,
                format!("Failed to write file '{}': {}", input.path, e),
            ));
        }

        let return_value = format!(
            "Successfully applied {} edits to file '{}':\n{}",
            input.edits.len(),
            input.path,
            edit_results.join("\n")
        );

        Ok(ToolOutcome {
            content: vec![UserContent::text(return_value)],
            details: ToolDetails::Diff {
                path: display_path,
                before: original_content,
                after: content,
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

/// Build a [`ToolOutcome`] for a recoverable error. The model gets
/// the human-readable error string as the tool result and
/// `is_error: true` so it can correct the call; the user sees the
/// same string in the CLI's error rendering via the bridge. The
/// summary falls back to the raw path so even non-absolute or
/// otherwise-unusable paths surface something meaningful in
/// collapsed views.
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

    /// Multiple independent edits applied in order. Confirms the wire
    /// content carries the per-edit summary lines and the structured
    /// `Diff` carries the original content as `before` and the
    /// post-batch content as `after`.
    #[tokio::test]
    async fn multiple_edits_apply_sequentially() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "alpha beta gamma\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileMultiTool
            .execute(
                &mut ctx,
                EditFileMultiInput {
                    path: path.display().to_string(),
                    edits: vec![
                        EditOperation {
                            old_string: "alpha".to_string(),
                            new_string: "ALPHA".to_string(),
                            replace_all: false,
                        },
                        EditOperation {
                            old_string: "gamma".to_string(),
                            new_string: "GAMMA".to_string(),
                            replace_all: false,
                        },
                    ],
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("Successfully applied 2 edits"),
            "wire: {wire:?}"
        );
        assert!(wire.contains("Edit #1"), "wire: {wire:?}");
        assert!(wire.contains("Edit #2"), "wire: {wire:?}");

        match &outcome.details {
            ToolDetails::Diff { before, after, .. } => {
                assert_eq!(before, "alpha beta gamma\n");
                assert_eq!(after, "ALPHA beta GAMMA\n");
            }
            other => panic!("expected Diff details, got {other:?}"),
        }

        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "ALPHA beta GAMMA\n");
    }

    /// Each subsequent edit sees the result of the previous one. Here
    /// the second edit only matches because the first edit produced
    /// the string it's looking for — exercises the "sequential
    /// against the in-memory copy" contract.
    #[tokio::test]
    async fn later_edits_see_results_of_earlier_ones() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "foo\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileMultiTool
            .execute(
                &mut ctx,
                EditFileMultiInput {
                    path: path.display().to_string(),
                    edits: vec![
                        EditOperation {
                            old_string: "foo".to_string(),
                            new_string: "intermediate".to_string(),
                            replace_all: false,
                        },
                        EditOperation {
                            old_string: "intermediate".to_string(),
                            new_string: "final".to_string(),
                            replace_all: false,
                        },
                    ],
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Diff { before, after, .. } => {
                assert_eq!(before, "foo\n");
                assert_eq!(after, "final\n");
            }
            other => panic!("expected Diff details, got {other:?}"),
        }
    }

    /// `replace_all: true` on a single edit replaces every occurrence
    /// in that edit only.
    #[tokio::test]
    async fn replace_all_applies_to_one_edit() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "x x y y\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileMultiTool
            .execute(
                &mut ctx,
                EditFileMultiInput {
                    path: path.display().to_string(),
                    edits: vec![EditOperation {
                        old_string: "x".to_string(),
                        new_string: "X".to_string(),
                        replace_all: true,
                    }],
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        match &outcome.details {
            ToolDetails::Diff { before, after, .. } => {
                assert_eq!(before, "x x y y\n");
                assert_eq!(after, "X X y y\n");
            }
            other => panic!("expected Diff details, got {other:?}"),
        }
    }

    /// Non-absolute paths surface as recoverable error outcomes
    /// rather than a hard `Err`, so the model can correct its call.
    #[tokio::test]
    async fn relative_path_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = EditFileMultiTool
            .execute(
                &mut ctx,
                EditFileMultiInput {
                    path: "relative/file.txt".to_string(),
                    edits: vec![EditOperation {
                        old_string: "x".to_string(),
                        new_string: "y".to_string(),
                        replace_all: false,
                    }],
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
        let outcome = EditFileMultiTool
            .execute(
                &mut ctx,
                EditFileMultiInput {
                    path: "/nonexistent/path/that/should/not/exist.txt".to_string(),
                    edits: vec![EditOperation {
                        old_string: "x".to_string(),
                        new_string: "y".to_string(),
                        replace_all: false,
                    }],
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

    /// A failed validation mid-batch leaves the file untouched: the
    /// disk write only runs once every edit has validated. This is
    /// the "all edits applied atomically" guarantee from the tool
    /// description.
    #[tokio::test]
    async fn validation_failure_mid_batch_leaves_file_untouched() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "alpha beta gamma\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileMultiTool
            .execute(
                &mut ctx,
                EditFileMultiInput {
                    path: path.display().to_string(),
                    edits: vec![
                        // First edit succeeds in-memory.
                        EditOperation {
                            old_string: "alpha".to_string(),
                            new_string: "ALPHA".to_string(),
                            replace_all: false,
                        },
                        // Second edit fails: no occurrence.
                        EditOperation {
                            old_string: "missing".to_string(),
                            new_string: "irrelevant".to_string(),
                            replace_all: false,
                        },
                    ],
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.contains("Edit #2"), "body: {body:?}");
                assert!(body.contains("No occurrences of"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }

        // File should be unchanged.
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "alpha beta gamma\n");
    }

    /// Ambiguous matches without `replace_all` surface as a
    /// recoverable error and leave the file untouched.
    #[tokio::test]
    async fn multiple_matches_without_replace_all_returns_error_outcome() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "foo foo foo\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileMultiTool
            .execute(
                &mut ctx,
                EditFileMultiInput {
                    path: path.display().to_string(),
                    edits: vec![EditOperation {
                        old_string: "foo".to_string(),
                        new_string: "bar".to_string(),
                        replace_all: false,
                    }],
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

        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "foo foo foo\n");
    }

    /// Locks in `Sequential` execution mode — the agent's batching
    /// logic relies on this to serialize filesystem mutations
    /// (`docs/aj-next-plan.md` §1.3).
    #[test]
    fn execution_mode_is_sequential() {
        assert_eq!(
            EditFileMultiTool.execution_mode(),
            ExecutionMode::Sequential
        );
    }
}
