//! `read_file` builtin — first tool migrated to the new
//! [`aj_agent::tool::ToolDefinition`] surface (`docs/aj-next-plan.md` §2.2).
//!
//! Returns a [`ToolOutcome`] with [`ToolDetails::Text`]: the `summary`
//! is the relative display path (with optional `start:end` line range)
//! and the `body` is the line-numbered content the user sees. The
//! `content` block sent back to the model preserves the original
//! line numbers so the LLM can reference them.
//!
//! The output is bounded by two simultaneous budgets (line count and
//! byte count) enforced by `truncate_head`: whichever fires first
//! wins. When clipped, the result carries an actionable footer telling
//! the model how to paginate; when a single line alone exceeds the
//! byte budget the body becomes an escape pointing at a
//! `sed | head -c` fallback.

use aj_agent::tool::{ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::{fs, path::PathBuf};

use crate::truncate::{READ_MAX_BYTES, READ_MAX_LINES, TruncatedBy, format_size, truncate_head};

const DESCRIPTION: &str = r#"
Read the contents of a file from the local file system. If a file does not exist
an error will be returned.

Usage:

- The path parameter must be an absolute path
- Results include line numbers, starting at 1
- Output is capped at 2000 lines or 50KB (whichever fires first). When the
  cap is hit, the result tells you the next offset to continue from
- You can specify an offset and a limit but it's usually better to read the
  whole file. Use this for reading very big files
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
        let total_file_lines = lines.len();

        // The model's `offset`/`limit` describe a slice over `lines`.
        // We apply both before truncation so a small explicit limit
        // wins over our auto-cap.
        let start_idx = input.offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
        let user_limited = input.limit.is_some();
        let user_end_idx = match input.limit {
            Some(limit) => (start_idx + limit).min(lines.len()),
            None => lines.len(),
        };

        let display_path_bare = display_relative(path, &ctx.working_directory());

        // Out-of-range offset: keep the legacy behaviour (empty body, no
        // line-range suffix, no footer).
        if start_idx >= lines.len() {
            return Ok(ToolOutcome {
                content: vec![UserContent::text(String::new())],
                details: ToolDetails::Text {
                    summary: display_path_bare,
                    body: String::new(),
                },
                is_error: false,
            });
        }

        let slice = &lines[start_idx..user_end_idx];
        let raw: String = slice.join("\n");
        let trunc = truncate_head(&raw, READ_MAX_LINES, READ_MAX_BYTES);

        // First-line-exceeds-limit: the single line at `start_idx` is
        // bigger than the byte cap on its own. Refuse to render a
        // partial line; point the model at a bash escape so it can
        // pull the bytes it needs with explicit framing.
        if trunc.first_line_exceeds_limit {
            let line_size = slice.first().map(|l| l.len()).unwrap_or(0);
            let start_line_display = start_idx + 1;
            let escape = format!(
                "[Line {start_line_display} is {}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {} | head -c {}]",
                format_size(line_size),
                format_size(READ_MAX_BYTES),
                input.path,
                READ_MAX_BYTES,
            );
            return Ok(ToolOutcome {
                content: vec![UserContent::text(escape.clone())],
                details: ToolDetails::Text {
                    summary: display_path_bare,
                    body: escape,
                },
                // Recoverable result, not is_error: the model can
                // act on the escape directly.
                is_error: false,
            });
        }

        let kept_count = trunc.output_lines;
        let kept = &slice[..kept_count];
        let start_line_display = start_idx + 1;
        let end_line_display = start_line_display + kept_count.saturating_sub(1);

        // Build the wire- and display-bound bodies from the kept lines.
        // The wire body preserves absolute line numbers so the model
        // can reference them; the display body renumbers from 1 to
        // match the legacy UI.
        let formatted_for_model: Vec<String> = kept
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:5>}: {}", start_idx + i + 1, line))
            .collect();
        let mut model_body = formatted_for_model.join("\n");
        let mut display_body = format_for_display(kept);

        // Footers — wire content and display body get the same string,
        // appended after a blank line for readability.
        let footer = if trunc.truncated {
            let next_offset = end_line_display + 1;
            match trunc.truncated_by {
                Some(TruncatedBy::Lines) => Some(format!(
                    "[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
                )),
                Some(TruncatedBy::Bytes) => Some(format!(
                    "[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                    format_size(READ_MAX_BYTES),
                )),
                // `truncated == true` always carries a reason; treat
                // an absent label as a no-op rather than panic.
                None => None,
            }
        } else if user_limited && start_idx + kept_count < total_file_lines {
            // The user's explicit `limit` stopped early but the file
            // has more content. Surface a continuation hint.
            let remaining = total_file_lines - (start_idx + kept_count);
            let next_offset = start_idx + kept_count + 1;
            Some(format!(
                "[{remaining} more lines in file. Use offset={next_offset} to continue.]"
            ))
        } else {
            None
        };

        if let Some(footer) = footer {
            model_body.push_str("\n\n");
            model_body.push_str(&footer);
            if !display_body.ends_with('\n') {
                display_body.push('\n');
            }
            display_body.push('\n');
            display_body.push_str(&footer);
            display_body.push('\n');
        }

        // Summary path keeps its existing `start:end` suffix when the
        // model narrowed the slice. The cap doesn't change that;
        // start/end here describe the actually-shown range.
        let mut display_path = display_path_bare;
        if input.offset.is_some() || input.limit.is_some() {
            display_path.push_str(&format!(" {start_line_display}:{end_line_display}"));
        }

        Ok(ToolOutcome {
            content: vec![UserContent::text(model_body)],
            details: ToolDetails::Text {
                summary: display_path,
                body: display_body,
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
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("1: alpha"), "wire content: {wire:?}");
        assert!(wire.contains("3: gamma"), "wire content: {wire:?}");
        assert!(
            !wire.contains("[Showing lines"),
            "small file should not have a footer: {wire:?}"
        );

        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                assert!(
                    !summary.contains(':'),
                    "unexpected line range in summary: {summary:?}"
                );
                assert!(body.contains("1: alpha"), "display body: {body:?}");
                assert!(body.contains("3: gamma"), "display body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

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
        assert!(wire.contains("3: line 3"), "wire: {wire:?}");
        assert!(wire.contains("4: line 4"), "wire: {wire:?}");
        assert!(
            !wire.contains("line 5"),
            "wire shouldn't include line 5: {wire:?}"
        );
        // 2-line slice from a 10-line file: continuation hint should
        // tell the model there are more lines and how to fetch them.
        assert!(
            wire.contains("more lines in file"),
            "expected user-limit hint: {wire:?}"
        );
        assert!(wire.contains("offset=5"), "wire: {wire:?}");

        match &outcome.details {
            ToolDetails::Text { summary, body } => {
                assert!(summary.ends_with(" 3:4"), "summary: {summary:?}");
                assert!(body.starts_with("1: line 3"), "body: {body:?}");
                assert!(body.contains("2: line 4"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

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

    /// A file longer than `READ_MAX_LINES` triggers the line-limited
    /// footer and tells the model the next offset.
    #[tokio::test]
    async fn large_line_count_emits_line_limited_footer() {
        let mut file = NamedTempFile::new().expect("temp file");
        for i in 1..=READ_MAX_LINES + 50 {
            writeln!(file, "line {i}").unwrap();
        }
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
        let wire = extract_text(&outcome.content);
        let expected_total = READ_MAX_LINES + 50;
        let expected_next = READ_MAX_LINES + 1;
        let expected_footer = format!(
            "[Showing lines 1-{READ_MAX_LINES} of {expected_total}. Use offset={expected_next} to continue.]"
        );
        assert!(
            wire.contains(&expected_footer),
            "missing line-limited footer\nfooter: {expected_footer}\nwire tail: {:?}",
            &wire[wire.len().saturating_sub(200)..]
        );
        // And the last shown wire line should be `line 2000`.
        assert!(wire.contains(&format!("{READ_MAX_LINES}: line {READ_MAX_LINES}")));
    }

    /// A file with one huge line under the line cap but over the byte
    /// cap triggers the byte-limited footer; the kept content is what
    /// fits under 50KB.
    #[tokio::test]
    async fn large_byte_count_emits_byte_limited_footer() {
        let mut file = NamedTempFile::new().expect("temp file");
        // Each line is ~6 KB; ten such lines = 60 KB total. The byte
        // cap (50 KB) fires before the line cap (2000).
        let chunk: String = "x".repeat(6 * 1024);
        for _ in 0..10 {
            writeln!(file, "{chunk}").unwrap();
        }
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
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("50.0KB limit"),
            "expected byte-limited footer\nwire tail: {:?}",
            &wire[wire.len().saturating_sub(200)..]
        );
        assert!(wire.contains("Use offset="));
    }

    /// A file whose first line alone exceeds the byte cap yields the
    /// escape message and nothing else; not flagged as an error (the
    /// model can act on the escape).
    #[tokio::test]
    async fn single_huge_line_emits_bash_escape_message() {
        let mut file = NamedTempFile::new().expect("temp file");
        let huge: String = "z".repeat(60 * 1024);
        // No trailing newline — keep the single-line shape.
        write!(file, "{huge}").unwrap();
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
        let wire = extract_text(&outcome.content);
        assert!(
            wire.starts_with("[Line 1 is "),
            "expected escape preface, got: {:?}",
            &wire[..wire.len().min(120)]
        );
        assert!(
            wire.contains("exceeds 50.0KB limit"),
            "wire: {:?}",
            &wire[..wire.len().min(160)]
        );
        assert!(
            wire.contains("sed -n '1p'") && wire.contains("head -c 51200"),
            "wire: {:?}",
            &wire[..wire.len().min(160)]
        );
        // The escape message must not contain the source bytes. Random
        // temp-file path suffixes can incidentally include the same
        // letter ('z' here), so check the size rather than the
        // character: a 60KB-of-z body would be vastly larger than
        // any plausible escape message.
        assert!(
            wire.len() < 1024,
            "escape should not leak file bytes (len={}): {:?}",
            wire.len(),
            &wire[..wire.len().min(200)],
        );
    }

    /// Offset that lands on a line bigger than the cap still routes to
    /// the escape; the escape's line number reflects the actual line.
    #[tokio::test]
    async fn offset_lands_on_huge_line_routes_to_escape() {
        let mut file = NamedTempFile::new().expect("temp file");
        writeln!(file, "small").unwrap();
        writeln!(file, "small").unwrap();
        let huge: String = "z".repeat(60 * 1024);
        writeln!(file, "{huge}").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool
            .execute(
                &mut ctx,
                ReadFileInput {
                    path: path.display().to_string(),
                    offset: Some(3),
                    limit: None,
                },
            )
            .await
            .expect("execute");

        let wire = extract_text(&outcome.content);
        assert!(wire.starts_with("[Line 3 is "), "wire: {wire:?}");
        assert!(wire.contains("sed -n '3p'"), "wire: {wire:?}");
    }
}
