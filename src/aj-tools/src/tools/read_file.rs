//! `read_file` builtin — first tool migrated to the new
//! [`aj_agent::tool::ToolDefinition`] surface (`docs/aj-next-plan.md` §2.2).
//!
//! Returns a [`ToolOutcome`] with [`ToolDetails::Text`]: the `summary`
//! is the relative display path (with optional `start:end` line range)
//! and the `body` is the line-numbered content the user sees. The
//! `content` block sent back to the model preserves the original
//! line numbers so the LLM can reference them.

use aj_agent::tool::{ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::{fs, path::PathBuf};

const DESCRIPTION: &str = r#"
Read the contents of a file from the local file system. If a file does not exist
an error will be returned.

Usage:

- The path parameter must be an absolute path
- Results include line numbers, starting at 1
- You can specify an offset and a limit but it's usually better to read the
  whole file. Use this for reading very big files.
"#;

#[derive(Clone)]
pub struct ReadFileTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct ReadFileInput {
    /// The absolute path to the file to read.
    path: String,
    /// The line number to start reading from (1-indexed). If not provided, starts from the beginning.
    #[serde(default)]
    offset: Option<usize>,
    /// The number of lines to read. If not provided, reads all lines from offset to end.
    #[serde(default)]
    limit: Option<usize>,
}

impl ToolDefinition for ReadFileTool {
    type Input = ReadFileInput;

    fn name(&self) -> &'static str {
        "read_file"
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
                &input.path,
                format!("Path must be absolute, got: {}", input.path),
            ));
        }

        let content = match fs::read_to_string(&input.path) {
            Ok(content) => content,
            Err(e) => {
                return Ok(error_outcome(
                    &input.path,
                    format!("Failed to read file '{}': {}", input.path, e),
                ));
            }
        };

        let lines: Vec<&str> = content.lines().collect();

        // Calculate start and end indices.
        let start_idx = input.offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
        let end_idx = match input.limit {
            Some(limit) => (start_idx + limit).min(lines.len()),
            None => lines.len(),
        };

        // Build a display path (relative to the working directory) and
        // append a `start:end` line-range suffix when offset/limit
        // narrow the slice. This is what users see in the tool header.
        let mut display_path = display_relative(path, &ctx.working_directory());
        if input.offset.is_some() || input.limit.is_some() {
            let start_line = start_idx + 1;
            let end_line = end_idx;
            display_path.push_str(&format!(" {start_line}:{end_line}"));
        }

        // Out-of-range offset: no lines, no body. The model gets an
        // empty `tool_result`; the user sees the header with no body.
        if start_idx >= lines.len() {
            return Ok(ToolOutcome {
                content: vec![UserContent::text(String::new())],
                details: ToolDetails::Text {
                    summary: display_path,
                    body: String::new(),
                },
                is_error: false,
            });
        }

        // For the model, preserve the original line numbers so it can
        // reference them.
        let formatted_for_model: Vec<String> = lines[start_idx..end_idx]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:5>}: {}", start_idx + i + 1, line))
            .collect();
        let return_value = formatted_for_model.join("\n");

        // For the user, the historical UI numbers the displayed lines
        // from 1 regardless of offset. Preserve that exactly.
        let formatted_for_display = format_for_display(&lines[start_idx..end_idx]);

        Ok(ToolOutcome {
            content: vec![UserContent::text(return_value)],
            details: ToolDetails::Text {
                summary: display_path,
                body: formatted_for_display,
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

/// Build a `ToolOutcome` for a recoverable error. The model gets the
/// human-readable error string as the tool result and `is_error: true`
/// so it can correct the call; the user sees the same string in the
/// CLI's error rendering via the bridge.
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

/// Formats `read_file` results for display to the user by adding line numbers.
pub fn format_for_display(lines: &[&str]) -> String {
    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        result.push_str(&format!("{:5>}: {}\n", i + 1, line));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;
    use aj_models::types::UserContent;
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

    /// Reads a small temp file end-to-end through the new contract and
    /// asserts the structured details + wire content match what the
    /// LLM and UI expect.
    #[tokio::test]
    async fn execute_reads_file_and_returns_text_outcome() {
        let mut file = NamedTempFile::new().expect("temp file");
        writeln!(file, "alpha").unwrap();
        writeln!(file, "beta").unwrap();
        writeln!(file, "gamma").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool
            .execute(
                &mut ctx,
                ReadFileInput {
                    path: path.display().to_string(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        // The wire content carries absolute line numbers so the model
        // can reference them.
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("1: alpha"), "wire content: {wire:?}");
        assert!(wire.contains("3: gamma"), "wire content: {wire:?}");

        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                // No offset/limit → no line-range suffix.
                assert!(
                    !summary.contains(':'),
                    "unexpected line range in summary: {summary:?}"
                );
                // The display body always renumbers from 1.
                assert!(body.contains("1: alpha"), "display body: {body:?}");
                assert!(body.contains("3: gamma"), "display body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Offset+limit narrows the slice but the model still sees the
    /// original line numbers; the display body renumbers from 1; the
    /// summary picks up a `start:end` suffix.
    #[tokio::test]
    async fn execute_honors_offset_and_limit() {
        let mut file = NamedTempFile::new().expect("temp file");
        for i in 1..=10 {
            writeln!(file, "line {i}").unwrap();
        }
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool
            .execute(
                &mut ctx,
                ReadFileInput {
                    path: path.display().to_string(),
                    offset: Some(3),
                    limit: Some(2),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        // Lines 3 and 4 only; numbered with their original line numbers.
        assert!(wire.contains("3: line 3"), "wire: {wire:?}");
        assert!(wire.contains("4: line 4"), "wire: {wire:?}");
        assert!(
            !wire.contains("line 5"),
            "wire shouldn't include line 5: {wire:?}"
        );

        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                assert!(summary.ends_with(" 3:4"), "summary: {summary:?}");
                // Display body renumbers from 1.
                assert!(body.starts_with("1: line 3"), "body: {body:?}");
                assert!(body.contains("2: line 4"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    /// Non-absolute paths surface as a recoverable error outcome
    /// rather than a hard `Err`, so the model can correct its call.
    #[tokio::test]
    async fn relative_path_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool
            .execute(
                &mut ctx,
                ReadFileInput {
                    path: "relative/file.txt".to_string(),
                    offset: None,
                    limit: None,
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

    /// Missing files surface as an error outcome — same recoverable
    /// shape as the absolute-path check so the model can retry with a
    /// corrected path.
    #[tokio::test]
    async fn missing_file_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool
            .execute(
                &mut ctx,
                ReadFileInput {
                    path: "/nonexistent/path/that/should/not/exist".to_string(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.contains("Failed to read file"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }
}
