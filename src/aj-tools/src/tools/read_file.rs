//! `read_file` builtin — first tool migrated to the new
//! [`aj_agent::tool::ToolDefinition`] surface (`docs/aj-next-plan.md` §2.2).
//!
//! For text files: returns a [`ToolOutcome`] with [`ToolDetails::Text`].
//! The `summary` is the relative display path (with optional `start:end`
//! line range) and the `body` is the line-numbered content the user
//! sees. The `content` block sent back to the model preserves the
//! original line numbers so the LLM can reference them.
//!
//! Text output is bounded by two simultaneous budgets (line count and
//! byte count) enforced by `truncate_head`: whichever fires first
//! wins. When clipped, the result carries an actionable footer telling
//! the model how to paginate; when a single line alone exceeds the
//! byte budget the body becomes an escape pointing at a
//! `sed | head -c` fallback.
//!
//! For supported image files (PNG, JPEG, GIF, WebP): returns a
//! [`ToolOutcome`] with [`ToolDetails::Image`]. The `content` is a
//! short text annotation followed by a [`UserContent::Image`]
//! attachment carrying the (possibly resized) image bytes. The
//! line-based `offset` / `limit` parameters are rejected on image
//! paths.

use aj_agent::tool::{ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::{fs, path::PathBuf};

use crate::image::{self, ResizeOptions, ResizedImage};
use crate::truncate::{READ_MAX_BYTES, READ_MAX_LINES, TruncatedBy, format_size, truncate_head};

const DESCRIPTION: &str = r#"
Read the contents of a file from the local file system. If a file does not exist
an error will be returned.

Usage:

- The path parameter must be an absolute path
- Supports text files and images (PNG, JPEG, GIF, WebP). Images are returned as
  attachments; the offset/limit parameters do not apply to images.
- For text files: results include line numbers, starting at 1. Output is capped
  at 2000 lines or 50KB (whichever fires first). When the cap is hit, the
  result tells you the next offset to continue from.
- You can specify an offset and a limit but it's usually better to read the
  whole file. Use this for reading very big files
"#;

#[derive(Clone)]
pub struct ReadFileTool {
    /// Whether to resize images to fit the inline image budget
    /// before attaching them to tool results. When `false`, the
    /// raw source bytes are base64-encoded and attached as-is.
    auto_resize: bool,
}

impl ReadFileTool {
    /// Construct with the default policy: auto-resize enabled.
    pub fn new() -> Self {
        Self { auto_resize: true }
    }

    /// Construct with an explicit resize policy. `false` skips the
    /// inline budget enforcement entirely; see
    /// [`crate::image::passthrough_image`] for the trade-off.
    pub fn with_auto_resize(auto_resize: bool) -> Self {
        Self { auto_resize }
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

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

        let display_path_bare = display_relative(path, &ctx.working_directory());
        if let Some(source_mime) = image::detect_mime_type_from_file(path) {
            // Non-vision warning omitted: `aj_models::transform` already substitutes
            // a placeholder when the target model can't see images, so the model
            // never receives a broken attachment.
            if input.offset.is_some() || input.limit.is_some() {
                return Ok(error_outcome(
                    &display_path_bare,
                    "offset/limit are not supported for image files".to_string(),
                ));
            }
            return Ok(read_image_outcome(
                input.path.clone(),
                display_path_bare,
                source_mime,
                self.auto_resize,
            )
            .await);
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
            .map(|(i, line)| format!("{:>5}: {}", start_idx + i + 1, line))
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

/// Read an image file from disk, resize it under the inline image
/// budget, and build the corresponding tool outcome.
///
/// Contract: `source_mime` must come from
/// [`image::detect_mime_type_from_file`] for `path` (i.e. the caller
/// has already confirmed this is a supported image). On a resize
/// failure (e.g. unreadable bytes), returns a recoverable error
/// outcome. On a budget-exhaustion failure, returns a recoverable
/// non-error outcome whose text annotation tells the model the
/// attachment was dropped.
///
/// The FS read, image decode/resize/encode, and outcome construction
/// all run on `tokio::task::spawn_blocking` because
/// [`image::resize_image`] and [`image::passthrough_image`] are
/// synchronous CPU-bound work (Lanczos3 resample + PNG/JPEG encode)
/// that can occupy a Tokio worker for tens of milliseconds on a
/// large source.
async fn read_image_outcome(
    path: String,
    display_path: String,
    source_mime: &'static str,
    auto_resize: bool,
) -> ToolOutcome {
    // Retained for the join-error arm: if the blocking closure panics,
    // `path` was moved into it and we can't recover it from the join
    // result.
    let path_for_join = path.clone();

    let result = tokio::task::spawn_blocking(move || {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                return BlockingOutcome::Error {
                    path,
                    message: format!("{e}"),
                };
            }
        };

        if !auto_resize {
            // Passthrough: attach raw source bytes. Decoding only happens
            // far enough to recover dimensions for `ToolDetails::Image`.
            return match image::passthrough_image(&bytes, source_mime) {
                Some(resized) => {
                    BlockingOutcome::Attachment(image_attachment_outcome(display_path, resized))
                }
                None => BlockingOutcome::Error {
                    path,
                    message: "could not decode image for dimension metadata".to_string(),
                },
            };
        }

        match image::resize_image(&bytes, source_mime, &ResizeOptions::default()) {
            Some(resized) => {
                BlockingOutcome::Attachment(image_attachment_outcome(display_path, resized))
            }
            None => BlockingOutcome::Omitted(image_omitted_outcome(display_path, source_mime)),
        }
    })
    .await;

    match result {
        Ok(BlockingOutcome::Attachment(outcome)) | Ok(BlockingOutcome::Omitted(outcome)) => outcome,
        Ok(BlockingOutcome::Error { path, message }) => {
            error_outcome(&path, format!("Failed to read file '{path}': {message}"))
        }
        // A panic on the blocking pool shouldn't kill the agent.
        // Surface it as a recoverable tool error pinned to the
        // original path.
        Err(join_err) => error_outcome(
            &path_for_join,
            format!("Failed to read file '{path_for_join}': image decode task failed: {join_err}"),
        ),
    }
}

/// Result carried back from the `spawn_blocking` closure inside
/// [`read_image_outcome`]. Keeping this as a small enum (rather than
/// `Result<Result<_, _>, _>`) makes the three outcomes explicit:
/// inline attachment, recoverable omission, or hard failure that
/// still needs the original path string to build a useful error.
enum BlockingOutcome {
    /// Image bytes were attached to the tool outcome successfully.
    Attachment(ToolOutcome),
    /// Image couldn't fit the inline-image budget; outcome carries
    /// the textual `[Image omitted: ...]` body. Not an error.
    Omitted(ToolOutcome),
    /// Hard failure reading or decoding; the path is returned so the
    /// async caller can build an `error_outcome` whose body matches
    /// the established `"Failed to read file '{path}': {reason}"`
    /// wording.
    Error { path: String, message: String },
}

/// Outcome for a successful image read: the resized image rides on
/// `content` as a [`UserContent::Image`] preceded by a text annotation
/// (`Read image file [<mime>]` plus an optional dimension note), and
/// [`ToolDetails::Image`] carries the metadata the TUI needs to render
/// or fall back gracefully.
fn image_attachment_outcome(display_path: String, resized: ResizedImage) -> ToolOutcome {
    let mut annotation = format!("Read image file [{}]", resized.mime_type);
    if let Some(note) = image::format_dimension_note(&resized) {
        annotation.push('\n');
        annotation.push_str(&note);
    }
    let mime_type = resized.mime_type.clone();
    let original_dimensions = (resized.original_width, resized.original_height);
    let displayed_dimensions = (resized.width, resized.height);
    ToolOutcome {
        content: vec![
            UserContent::text(annotation),
            UserContent::image(resized.data, resized.mime_type),
        ],
        details: ToolDetails::Image {
            summary: display_path,
            mime_type,
            original_dimensions,
            displayed_dimensions,
        },
        is_error: false,
    }
}

/// Outcome when no encoding fits the inline image budget at any
/// dimension. Recoverable (`is_error: false`): the model is told the
/// attachment was dropped and can decide what to do next.
fn image_omitted_outcome(display_path: String, source_mime: &str) -> ToolOutcome {
    let body = format!(
        "Read image file [{source_mime}]\n[Image omitted: could not be resized below the inline image size limit.]"
    );
    ToolOutcome {
        content: vec![UserContent::text(body.clone())],
        details: ToolDetails::Text {
            summary: display_path,
            body,
        },
        is_error: false,
    }
}

/// Formats `read_file` results for display to the user by adding line numbers.
pub fn format_for_display(lines: &[&str]) -> String {
    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        result.push_str(&format!("{:>5}: {}\n", i + 1, line));
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
        let outcome = ReadFileTool::new()
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

    /// Pins the line-number gutter contract: numbers are right-aligned
    /// in a 5-wide column, so single- and multi-digit lines share a
    /// common separator position. `contains("1: ...")` cannot catch
    /// this, so we assert the padded prefixes exactly.
    #[tokio::test]
    async fn execute_right_aligns_line_number_gutter() {
        let mut file = NamedTempFile::new().expect("temp file");
        for i in 1..=10 {
            writeln!(file, "line {i}").unwrap();
        }
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool::new()
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

        let wire = extract_text(&outcome.content);
        assert!(wire.contains("    1: line 1"), "wire: {wire:?}");
        assert!(wire.contains("   10: line 10"), "wire: {wire:?}");

        let display = format_for_display(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
        assert!(display.contains("    1: a"), "display: {display:?}");
        assert!(display.contains("   10: j"), "display: {display:?}");
    }

    #[tokio::test]
    async fn execute_honors_offset_and_limit() {
        let mut file = NamedTempFile::new().expect("temp file");
        for i in 1..=10 {
            writeln!(file, "line {i}").unwrap();
        }
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool::new()
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
                assert!(body.starts_with("    1: line 3"), "body: {body:?}");
                assert!(body.contains("    2: line 4"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn relative_path_returns_error_outcome() {
        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool::new()
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
        let outcome = ReadFileTool::new()
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
        let outcome = ReadFileTool::new()
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
        let outcome = ReadFileTool::new()
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
        let outcome = ReadFileTool::new()
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
        let outcome = ReadFileTool::new()
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

    // -------------------------------------------------------------
    // Image branch
    // -------------------------------------------------------------

    /// Encode a `width`x`height` PNG with a faintly varying pattern.
    /// Mirrors the helper from `crate::image::tests::make_png` so the
    /// test stays self-contained without making test fixtures `pub`.
    fn make_png(width: u32, height: u32) -> Vec<u8> {
        use ::image::{ImageFormat, Rgba, RgbaImage};
        let mut img = RgbaImage::new(width, height);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = u8::try_from((x ^ y) & 0xff).unwrap_or(0);
            *px = Rgba([r, 128, 64, 255]);
        }
        let mut buf = Vec::new();
        ::image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .expect("encode png");
        buf
    }

    /// Solid-color PNG: compresses to a tiny payload regardless of
    /// dimensions, which lets large-image tests assume "PNG fits the
    /// budget" without doing the math.
    fn make_solid_png(width: u32, height: u32) -> Vec<u8> {
        use ::image::{ImageFormat, Rgba, RgbaImage};
        let img = RgbaImage::from_pixel(width, height, Rgba([180, 200, 220, 255]));
        let mut buf = Vec::new();
        ::image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .expect("encode png");
        buf
    }

    fn write_png_tempfile(bytes: &[u8]) -> NamedTempFile {
        let file = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .expect("temp file");
        std::fs::write(file.path(), bytes).expect("write png");
        file
    }

    #[tokio::test]
    async fn execute_returns_image_outcome_for_png() {
        let bytes = make_png(64, 48);
        let file = write_png_tempfile(&bytes);
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool::new()
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
        assert_eq!(
            outcome.content.len(),
            2,
            "expected text annotation + image block, got {:?}",
            outcome.content.len()
        );
        match &outcome.content[0] {
            UserContent::Text(t) => assert!(
                t.text.starts_with("Read image file [image/"),
                "annotation: {:?}",
                t.text
            ),
            other => panic!("expected text annotation first, got {other:?}"),
        }
        let image_mime = match &outcome.content[1] {
            UserContent::Image(img) => {
                assert!(!img.data.is_empty(), "image data must be non-empty");
                img.mime_type.clone()
            }
            other => panic!("expected image content second, got {other:?}"),
        };
        match &outcome.details {
            ToolDetails::Image {
                mime_type,
                original_dimensions,
                displayed_dimensions,
                ..
            } => {
                assert_eq!(mime_type, &image_mime);
                assert_eq!(original_dimensions, &(64, 48));
                assert_eq!(displayed_dimensions, &(64, 48));
            }
            other => panic!("expected ToolDetails::Image, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn small_image_has_no_dimension_note() {
        let bytes = make_png(32, 32);
        let file = write_png_tempfile(&bytes);
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool::new()
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

        let annotation = match &outcome.content[0] {
            UserContent::Text(t) => t.text.clone(),
            other => panic!("expected text annotation, got {other:?}"),
        };
        assert_eq!(annotation, "Read image file [image/png]");
        assert!(
            !annotation.contains("Multiply coordinates"),
            "annotation should not include dimension note: {annotation:?}"
        );
    }

    #[tokio::test]
    async fn large_image_includes_dimension_note() {
        // Solid color so PNG compresses below the budget even at
        // 4000x3000, guaranteeing the resize path runs and the note
        // is emitted.
        let bytes = make_solid_png(4000, 3000);
        let file = write_png_tempfile(&bytes);
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool::new()
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

        let annotation = match &outcome.content[0] {
            UserContent::Text(t) => t.text.clone(),
            other => panic!("expected text annotation, got {other:?}"),
        };
        assert!(
            annotation.starts_with("Read image file ["),
            "annotation: {annotation:?}"
        );
        assert!(
            annotation.contains("Multiply coordinates"),
            "expected dimension note in annotation: {annotation:?}"
        );
    }

    #[tokio::test]
    async fn offset_on_image_path_returns_error() {
        let bytes = make_png(32, 32);
        let file = write_png_tempfile(&bytes);
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool::new()
            .execute(
                &mut ctx,
                ReadFileInput {
                    path: path.display().to_string(),
                    offset: Some(2),
                    limit: None,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(
                    body.contains("offset/limit are not supported for image files"),
                    "body: {body:?}"
                );
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_image_with_auto_resize_disabled_attaches_raw_bytes() {
        // For a small PNG the raw source already fits the budget, so
        // resize_image's fast path also returns base64 of the source.
        // What this test pins is the contract: when auto_resize is
        // false we attach the source bytes verbatim, with
        // `displayed_dimensions == original_dimensions` and no
        // dimension annotation.
        let bytes = make_png(64, 48);
        let file = write_png_tempfile(&bytes);
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool::with_auto_resize(false)
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
        let image_data = match &outcome.content[1] {
            UserContent::Image(img) => img.data.clone(),
            other => panic!("expected image content second, got {other:?}"),
        };
        use base64::Engine;
        let expected = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert_eq!(
            image_data, expected,
            "passthrough must base64-encode the raw source bytes"
        );
        match &outcome.details {
            ToolDetails::Image {
                original_dimensions,
                displayed_dimensions,
                ..
            } => {
                assert_eq!(original_dimensions, &(64, 48));
                assert_eq!(displayed_dimensions, &(64, 48));
            }
            other => panic!("expected ToolDetails::Image, got {other:?}"),
        }
        // No dimension note when the image wasn't resized.
        let annotation = match &outcome.content[0] {
            UserContent::Text(t) => t.text.clone(),
            other => panic!("expected text annotation, got {other:?}"),
        };
        assert!(
            !annotation.contains("Multiply coordinates"),
            "annotation should not include dimension note: {annotation:?}"
        );
    }

    /// Pin the contract that the image branch is drivable from a
    /// single-thread Tokio runtime. `spawn_blocking` dispatches to a
    /// dedicated blocking pool independent of the worker count, so
    /// this is mostly a regression guard: if we ever revert to running
    /// the resize on the worker, a current-thread runtime would still
    /// complete, but this test documents the expected wiring.
    #[tokio::test(flavor = "current_thread")]
    async fn image_branch_runs_under_current_thread_runtime() {
        let bytes = make_png(16, 16);
        let file = write_png_tempfile(&bytes);
        let path = file.path().display().to_string();

        let outcome = read_image_outcome(path, "test.png".to_string(), "image/png", true).await;

        assert!(!outcome.is_error);
        assert!(matches!(&outcome.details, ToolDetails::Image { .. }));
    }

    #[tokio::test]
    async fn read_large_image_with_auto_resize_disabled_skips_omission_note() {
        // A large solid-color PNG. With auto_resize disabled the
        // resize ladder is bypassed entirely, so the result is an
        // image attachment (not the "[Image omitted]" placeholder).
        let bytes = make_solid_png(4000, 3000);
        let file = write_png_tempfile(&bytes);
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = ReadFileTool::with_auto_resize(false)
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
        assert!(
            matches!(&outcome.content[1], UserContent::Image(_)),
            "expected image attachment, not omission text"
        );
        assert!(matches!(&outcome.details, ToolDetails::Image { .. }));
        let annotation = match &outcome.content[0] {
            UserContent::Text(t) => t.text.clone(),
            other => panic!("expected text annotation, got {other:?}"),
        };
        assert!(
            !annotation.contains("Image omitted"),
            "passthrough must not emit the omission placeholder: {annotation:?}"
        );
    }
}
