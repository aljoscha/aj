//! `write_file` builtin — writes (or overwrites) a file on disk.
//!
//! Implements [`aj_agent::tool::ToolDefinition`]. Returns a
//! [`ToolOutcome`] whose
//! `details` is [`ToolDetails::Diff`] on success. It contains the
//! canonical compact display diff for the file edit. The wire `content` is
//! the short success summary so the model still sees a deterministic
//! `"Successfully {action} ..."` line.
//!
//! Recoverable errors (path-not-absolute, IO write failure) come back
//! as `is_error: true` outcomes carrying [`ToolDetails::Text`] so the
//! model can correct its call instead of aborting the turn.
//! [`execution_mode`] is overridden to [`ExecutionMode::Sequential`]
//! because this tool mutates the filesystem — the agent serializes a
//! batch containing it to avoid interleaved writes.
//!
//! [`execution_mode`]: ToolDefinition::execution_mode

use std::path::{Path, PathBuf};
use std::{fs, io};

use aj_agent::tool::{
    DiffDetails, ExecutionMode, ToolContext, ToolDefinition, ToolDetails, ToolOutcome,
};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DESCRIPTION: &str = r#"
Write a file to the local file system.

Usage:

- The path parameter must be an absolute path
- This will overwrite an existing file if there is one at the given path!
- Only create a new file when it is actually needed; don't create files proactively
- For an existing file, prefer edit_file even for extensive changes. Overwrite with this tool only when you are replacing nearly all of the file's content and the file is small (under ~250 lines)
- IMPORTANT: Don't use this tool for renaming a file. Prefer to use the bash tool with the mv command.
"#;

#[derive(Clone)]
pub struct WriteFileTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct WriteFileInput {
    /// The absolute path to the file to write.
    pub path: String,
    /// The content to write to the file.
    pub content: String,
}

impl ToolDefinition for WriteFileTool {
    type Input = WriteFileInput;

    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    /// `write_file` mutates the filesystem, so it runs in `Sequential`
    /// mode: a batch containing it serializes around any other
    /// in-flight tool calls.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        let path = Path::new(&input.path);
        if !path.is_absolute() {
            return Ok(error_outcome(
                &input.path,
                format!("Path must be absolute, got: {}", input.path),
            ));
        }

        // Read the previous content before writing so the display diff can be
        // constructed before the filesystem mutation.
        let original_content = match fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(_) => None,
        };
        let file_existed = original_content.is_some();

        let display_path = display_relative(path, &ctx.working_directory());

        // Build the display outcome before touching disk. Myers diffing is
        // bounded, but keeping all rendering work ahead of the write also
        // prevents an unexpected panic from leaving an unreported mutation.
        let details = ToolDetails::Diff(DiffDetails::new(
            display_path,
            original_content.as_deref().unwrap_or_default(),
            &input.content,
        ));

        if let Err(e) = fs::write(path, &input.content) {
            return Ok(error_outcome(
                &input.path,
                format!("Failed to write file '{}': {}", input.path, e),
            ));
        }

        let action = if file_existed { "overwrote" } else { "created" };
        let return_value = format!("Successfully {} file '{}'", action, input.path);

        Ok(ToolOutcome {
            content: vec![UserContent::text(return_value)],
            details,
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
    use aj_models::types::UserContent;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

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

    /// Writes a brand-new file and returns an all-addition display diff.
    #[tokio::test]
    async fn create_new_file_returns_diff_outcome() {
        let dir = TempDir::new().expect("temp dir");
        let target = dir.path().join("new.txt");

        let mut ctx = DummyToolContext::default();
        let outcome = WriteFileTool
            .execute(
                &mut ctx,
                WriteFileInput {
                    path: target.display().to_string(),
                    content: "hello\nworld\n".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.starts_with("Successfully created"), "wire: {wire:?}");
        assert!(
            wire.contains(&target.display().to_string()),
            "wire: {wire:?}"
        );

        match &outcome.details {
            ToolDetails::Diff(diff) => {
                assert!(diff.path().ends_with("new.txt"), "path: {:?}", diff.path());
                assert!(
                    diff.lines()
                        .iter()
                        .all(|line| !line.text().starts_with("--- a/")),
                    "creation should omit the old header"
                );
                assert!(diff.lines().iter().any(|line| line.text() == "+ hello"));
                assert!(diff.lines().iter().any(|line| line.text() == "+ world"));
            }
            other => panic!("expected Diff details, got {other:?}"),
        }

        // The file should actually be on disk now with the requested bytes.
        let on_disk = fs::read_to_string(&target).expect("read back");
        assert_eq!(on_disk, "hello\nworld\n");
    }

    /// Overwrites an existing file and returns its compact display diff.
    #[tokio::test]
    async fn overwrite_existing_file_returns_diff_outcome() {
        let mut file = NamedTempFile::new().expect("temp file");
        writeln!(file, "old line one").unwrap();
        writeln!(file, "old line two").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = WriteFileTool
            .execute(
                &mut ctx,
                WriteFileInput {
                    path: path.display().to_string(),
                    content: "new content\n".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.starts_with("Successfully overwrote"), "wire: {wire:?}");

        match &outcome.details {
            ToolDetails::Diff(diff) => {
                assert!(
                    diff.lines()
                        .iter()
                        .any(|line| line.text() == "- old line one")
                );
                assert!(
                    diff.lines()
                        .iter()
                        .any(|line| line.text() == "- old line two")
                );
                assert!(
                    diff.lines()
                        .iter()
                        .any(|line| line.text() == "+ new content")
                );
            }
            other => panic!("expected Diff details, got {other:?}"),
        }

        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "new content\n");
    }

    /// Non-absolute paths surface as a recoverable error outcome
    /// rather than a hard `Err`, so the model can correct its call.
    #[tokio::test]
    async fn relative_path_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = WriteFileTool
            .execute(
                &mut ctx,
                WriteFileInput {
                    path: "relative/file.txt".to_string(),
                    content: "irrelevant".to_string(),
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

    /// Write failures (e.g. parent directory missing) come back as a
    /// recoverable error outcome rather than bubbling an `Err`.
    #[tokio::test]
    async fn write_failure_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = WriteFileTool
            .execute(
                &mut ctx,
                WriteFileInput {
                    path: "/nonexistent/parent/that/should/not/exist/file.txt".to_string(),
                    content: "irrelevant".to_string(),
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.contains("Failed to write file"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Locks in `Sequential` execution mode — the agent's batching
    /// logic relies on this to serialize filesystem mutations.
    #[test]
    fn execution_mode_is_sequential() {
        assert_eq!(WriteFileTool.execution_mode(), ExecutionMode::Sequential);
    }
}
