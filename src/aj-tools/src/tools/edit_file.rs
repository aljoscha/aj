//! `edit_file` builtin: apply a single string replacement to a file.
//!
//! Implements [`aj_agent::tool::ToolDefinition`]. Returns a
//! [`ToolOutcome`] whose
//! `details` is [`ToolDetails::Diff`] on success. It contains the
//! canonical compact display diff for the replacement. The wire `content` is
//! the short success summary so the model still sees
//! a deterministic `"Successfully replaced ..."` line.
//!
//! Matching escalates from an exact substring match to a
//! whitespace-tolerant line fallback. See the `apply_edit` helper for
//! the details.
//!
//! Recoverable errors (path-not-absolute, file-not-found, read /
//! write failure, no match, ambiguous match, or an identical old/new
//! string) come back as `is_error: true` outcomes carrying
//! [`ToolDetails::Text`] so the model can correct its call instead of
//! aborting the turn. [`execution_mode`] is overridden to
//! [`ExecutionMode::Sequential`] because this tool mutates the
//! filesystem, so the agent serializes a batch containing it to avoid
//! interleaved writes.
//!
//! [`execution_mode`]: ToolDefinition::execution_mode

use std::fs;
use std::path::{Path, PathBuf};

use aj_agent::tool::{
    DiffDetails, ExecutionMode, ToolContext, ToolDefinition, ToolDetails, ToolOutcome,
};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DESCRIPTION: &str = r#"
Edit files by doing exact string replacement.

Usage:

- The path parameter must be an absolute path
- The file must exist
- Read the file first (for example with read_file) so old_string reflects its exact current content
- old_string and new_string must be different from each other
- old_string must match exactly one occurrence in the file, you can provide a larger string with more context to make it more unique, or use replace_all to replace all occurences
- If there are zero matches or multiple matches, the operation will fail
- If replace_all is set to true, all occurrences of old_string will be replaced with new_string
- To replace the entire contents of a file, use write_file instead; it uses fewer tokens because you don't repeat the existing content in old_string
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

    /// `edit_file` mutates the filesystem, so it runs in `Sequential`
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

        // Diff against the normalized original so a CRLF->LF normalization
        // is not mistaken for an edit. apply_edit normalizes internally and
        // derives its result from the same normalized text, so this base
        // matches what gets written.
        let normalized = normalize_newlines(&original_content);
        let new_content = match apply_edit(
            &original_content,
            &input.old_string,
            &input.new_string,
            input.replace_all,
        ) {
            Ok(content) => content,
            Err(err) => return Ok(error_outcome(&input.path, err.message(&input))),
        };

        let display_path = display_relative(path, &ctx.working_directory());
        // Complete rendering work before mutation so a diff timeout fallback or
        // unexpected panic cannot leave a successful write without an outcome.
        let details = ToolDetails::Diff(DiffDetails::new(display_path, &normalized, &new_content));

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
            details,
            is_error: false,
        })
    }
}

/// CRLF is normalized to LF before matching, and the normalized text is
/// what we write back. Editing a CRLF file therefore rewrites it with LF
/// line endings, which we accept: normalizing both sides is what lets a
/// model's LF-only `old_string` match a file saved with CRLF.
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// Undo one level of backslash-escaping. Some models emit the search or
/// replace text with literal escape sequences (`\n`, `\t`, `\\`, `\"`,
/// `\'`) instead of the real characters, and this maps them back. Both
/// `\n` and `\\n` end up as a real newline (likewise for tabs); the
/// dedicated double-backslash step only matters for backslashes not
/// followed by `n`/`t`.
fn unescape(s: &str) -> String {
    s.replace("\\\\n", "\\n")
        .replace("\\\\t", "\\t")
        .replace("\\\\", "\\")
        .replace("\\\"", "\"")
        .replace("\\'", "'")
        .replace("\\n", "\n")
        .replace("\\t", "\t")
}

/// Leading run of whitespace in `line`, or the whole line when it is
/// blank or all whitespace.
fn leading_whitespace(line: &str) -> &str {
    let end = line
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(line.len());
    &line[..end]
}

/// Whitespace-tolerant line replacement, used only after an exact match
/// fails. Slides a window the height of `old_string` over `content` and
/// takes the first window whose lines are all equal after trimming. The
/// replacement is re-indented: the matched block's own leading
/// whitespace (from its first line) is reapplied to each replacement
/// line while the replacement's relative indentation is preserved.
///
/// This is deliberately forgiving and applies at the first matching
/// window without checking for a second one.
fn fuzzy_line_replace(content: &str, old_string: &str, new_string: &str) -> Option<String> {
    let old_lines: Vec<&str> = old_string.split('\n').collect();
    let file_lines: Vec<&str> = content.split('\n').collect();
    if old_lines.len() > file_lines.len() {
        return None;
    }

    for start in 0..=(file_lines.len() - old_lines.len()) {
        let window = &file_lines[start..start + old_lines.len()];
        if !old_lines
            .iter()
            .zip(window)
            .all(|(o, f)| o.trim() == f.trim())
        {
            continue;
        }

        let file_indent = leading_whitespace(file_lines[start]);
        let new_lines: Vec<&str> = new_string.split('\n').collect();
        // Base indent is the leading whitespace run of the whole
        // new_string, so a leading blank line makes it span the newline.
        // When it does, no single line's indent starts with it, so every
        // line reanchors to file_indent. This mirrors the reference's
        // `/^\s*/` taken over the entire string rather than the first line.
        let base_indent = leading_whitespace(new_string);
        let rebuilt = new_lines.iter().map(|line| {
            if line.is_empty() {
                return String::new();
            }
            let indent = leading_whitespace(line);
            // Indentation beyond the replacement's own base indent is
            // relative and kept. Anything else is dropped so the block
            // reanchors to the file's actual indentation.
            let relative = indent.strip_prefix(base_indent).unwrap_or("");
            format!("{}{}{}", file_indent, relative, &line[indent.len()..])
        });

        let mut out: Vec<String> =
            Vec::with_capacity(file_lines.len() - old_lines.len() + new_lines.len());
        out.extend(file_lines[..start].iter().map(|l| l.to_string()));
        out.extend(rebuilt);
        out.extend(
            file_lines[start + old_lines.len()..]
                .iter()
                .map(|l| l.to_string()),
        );
        return Some(out.join("\n"));
    }

    None
}

/// Reason an edit could not be applied, rendered into a model-facing
/// message by the caller.
#[derive(Debug)]
enum EditError {
    /// `old_string` equals `new_string`, so the edit is a no-op.
    Identical,
    /// `old_string` occurs more than once and `replace_all` is unset.
    Ambiguous(usize),
    /// No exact, whitespace-tolerant, or unescaped match was found.
    NotFound,
}

impl EditError {
    fn message(&self, input: &EditFileInput) -> String {
        match self {
            EditError::Identical => {
                "old_string and new_string must be different from each other.".to_string()
            }
            EditError::Ambiguous(count) => format!(
                "Found {} occurrences of '{}' in file '{}'. Exactly one occurrence is required for safe replacement. Set replace_all to true to replace all occurrences.",
                count, input.old_string, input.path
            ),
            EditError::NotFound => format!(
                "No occurrences of '{}' found in file '{}'",
                input.old_string, input.path
            ),
        }
    }
}

/// Apply a single `old_string` -> `new_string` edit to `content` and
/// return the rewritten file. Line endings are normalized to LF on all
/// inputs before matching, so the returned content uses LF.
///
/// Matching escalates through three strategies:
///
/// 1. Exact substring. With `replace_all` every occurrence is replaced.
///    Otherwise the match must be unique, or the call is ambiguous.
/// 2. A whitespace-tolerant line fallback, only when the exact match
///    finds nothing: lines must match after trimming, and the
///    replacement is re-indented to the file's indentation.
/// 3. The same line fallback retried after undoing one level of
///    backslash-escaping, for models that emit literal `\n` / `\t`.
fn apply_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, EditError> {
    if old_string == new_string {
        return Err(EditError::Identical);
    }

    let content = normalize_newlines(content);
    let old = normalize_newlines(old_string);
    let new = normalize_newlines(new_string);

    if replace_all {
        if !content.contains(&old) {
            return Err(EditError::NotFound);
        }
        return Ok(content.replace(&old, &new));
    }

    if let Some(pos) = content.find(&old) {
        // A second non-overlapping occurrence makes the target ambiguous
        // unless the caller opted into replace_all.
        if content[pos + old.len()..].contains(&old) {
            return Err(EditError::Ambiguous(content.matches(&old).count()));
        }
        return Ok(content.replacen(&old, &new, 1));
    }

    if let Some(result) = fuzzy_line_replace(&content, &old, &new) {
        return Ok(result);
    }
    // Last resort: some models escape the strings. Undo one level and
    // retry the line fallback against the same normalized file.
    let old = normalize_newlines(&unescape(old_string));
    let new = normalize_newlines(&unescape(new_string));
    if let Some(result) = fuzzy_line_replace(&content, &old, &new) {
        return Ok(result);
    }

    Err(EditError::NotFound)
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

    /// Replaces one occurrence and returns its compact display diff.
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
            ToolDetails::Diff(diff) => {
                assert!(
                    diff.lines()
                        .iter()
                        .any(|line| line.text() == "- alpha beta gamma")
                );
                assert!(
                    diff.lines()
                        .iter()
                        .any(|line| line.text() == "+ alpha BETA gamma")
                );
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
            ToolDetails::Diff(diff) => {
                assert!(
                    diff.lines()
                        .iter()
                        .any(|line| line.text() == "- foo foo foo")
                );
                assert!(
                    diff.lines()
                        .iter()
                        .any(|line| line.text() == "+ bar bar bar")
                );
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

    /// A CRLF file matches an LF-only `old_string` and is written back
    /// as LF. Both sides are newline-normalized before matching.
    #[tokio::test]
    async fn crlf_file_matches_lf_old_string_and_writes_lf() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "one\r\ntwo\r\nthree\r\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: path.display().to_string(),
                    old_string: "two".to_string(),
                    new_string: "TWO".to_string(),
                    replace_all: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error, "outcome: {outcome:?}");
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "one\nTWO\nthree\n");
    }

    /// When the exact substring is absent only because of indentation,
    /// the whitespace-tolerant fallback matches on trimmed lines and
    /// re-indents the replacement to the file's actual indentation.
    #[tokio::test]
    async fn whitespace_tolerant_fallback_reindents_replacement() {
        let mut file = NamedTempFile::new().expect("temp file");
        // Body lines are indented two spaces, so the model's unindented
        // multi-line `old_string` is not an exact substring.
        write!(file, "fn f() {{\n  a();\n  b();\n}}\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: path.display().to_string(),
                    old_string: "a();\nb();".to_string(),
                    new_string: "a();\nc();".to_string(),
                    replace_all: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error, "outcome: {outcome:?}");
        let on_disk = fs::read_to_string(&path).expect("read back");
        // The replacement keeps the file's two-space indentation.
        assert_eq!(on_disk, "fn f() {\n  a();\n  c();\n}\n");
    }

    /// When nothing matches as-is, the tool retries after undoing one
    /// level of backslash-escaping, so a model that emitted literal
    /// `\n` still lands its edit.
    #[tokio::test]
    async fn escaped_old_string_matches_after_unescape() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "line1\nline2\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: path.display().to_string(),
                    // Literal backslash-n, not a real newline.
                    old_string: "line1\\nline2".to_string(),
                    new_string: "line1\\nLINE2".to_string(),
                    replace_all: false,
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error, "outcome: {outcome:?}");
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "line1\nLINE2\n");
    }

    /// An identical `old_string` / `new_string` is a no-op and surfaces
    /// as a recoverable error, leaving the file untouched.
    #[tokio::test]
    async fn identical_old_and_new_returns_error_outcome() {
        let mut file = NamedTempFile::new().expect("temp file");
        write!(file, "hello world\n").unwrap();
        let path = file.path().to_path_buf();

        let mut ctx = DummyToolContext::default();
        let outcome = EditFileTool
            .execute(
                &mut ctx,
                EditFileInput {
                    path: path.display().to_string(),
                    old_string: "hello".to_string(),
                    new_string: "hello".to_string(),
                    replace_all: false,
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        match &outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(body.contains("must be different"), "body: {body:?}");
            }
            other => panic!("expected Text details, got {other:?}"),
        }

        let on_disk = fs::read_to_string(&path).expect("read back");
        assert_eq!(on_disk, "hello world\n");
    }

    /// `unescape` turns literal escape sequences into their characters.
    #[test]
    fn unescape_turns_literal_sequences_into_characters() {
        assert_eq!(unescape("a\\nb"), "a\nb");
        assert_eq!(unescape("a\\tb"), "a\tb");
        // A doubled backslash collapses to one.
        assert_eq!(unescape("a\\\\b"), "a\\b");
        // An escaped quote becomes a quote.
        assert_eq!(unescape("a\\\"b"), "a\"b");
    }

    /// A `new_string` that begins with a blank line: the replacement's
    /// base indent is the whole string's leading whitespace run, which
    /// spans the blank line, so each real line reanchors to the file's
    /// indentation rather than keeping its own.
    #[test]
    fn fuzzy_reindent_with_leading_blank_line_in_new() {
        let out = apply_edit("  a\n  b\n", "a\nb", "\n    b2", false).unwrap();
        assert_eq!(out, "\n  b2\n");
    }

    /// A replacement line indented less than the replacement's own first
    /// line drops its indentation and reanchors to the file indent.
    #[test]
    fn fuzzy_reindent_drops_indent_not_under_base() {
        let out = apply_edit(
            "def outer():\n        inner()\n        cleanup()\n",
            "    inner()\n    cleanup()",
            "    inner()\n  done()",
            false,
        )
        .unwrap();
        assert_eq!(out, "def outer():\n        inner()\n        done()\n");
    }

    /// A deeper replacement line keeps its indentation relative to the
    /// replacement's base indent, added on top of the file indent.
    #[test]
    fn fuzzy_reindent_keeps_relative_deeper_indent() {
        let out = apply_edit("  a\n  b\n", "a\nb", "a\n    nested", false).unwrap();
        assert_eq!(out, "  a\n      nested\n");
    }

    /// replace_all normalizes CRLF and replaces every occurrence, writing
    /// LF back.
    #[test]
    fn replace_all_after_crlf_normalization() {
        let out = apply_edit("x\r\ny\r\nx\r\n", "x", "z", true).unwrap();
        assert_eq!(out, "z\ny\nz\n");
    }

    /// Locks in `Sequential` execution mode — the agent's batching
    /// logic relies on this to serialize filesystem mutations.
    #[test]
    fn execution_mode_is_sequential() {
        assert_eq!(EditFileTool.execution_mode(), ExecutionMode::Sequential);
    }
}
