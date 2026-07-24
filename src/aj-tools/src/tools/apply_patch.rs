//! Transport-independent implementation of the Codex/Amp `apply_patch` tool.

use std::fs;
use std::path::{Path, PathBuf};

use aj_agent::tool::{
    ExecutionMode, ToolContext, ToolDefinition, ToolDetails, ToolOutcome, wire_diff,
};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DESCRIPTION: &str = r#"Apply a patch to one or more files using the Codex patch format.

You MUST read the file before applying a patch to it.

## Patch Format

The patch must be wrapped in `*** Begin Patch` and `*** End Patch` markers.

Each operation starts with one of three headers:
- `*** Add File: <path>` creates a new file. Every following line must start with `+`.
- `*** Delete File: <path>` removes an existing file. Nothing follows.
- `*** Update File: <path>` patches an existing file, optionally with a rename via `*** Move to:`.

```text
Patch       := Begin { FileOp } End
Begin       := "*** Begin Patch" NEWLINE
End         := "*** End Patch" NEWLINE
FileOp      := AddFile | DeleteFile | UpdateFile
AddFile     := "*** Add File: " path NEWLINE { "+" line NEWLINE }
DeleteFile  := "*** Delete File: " path NEWLINE
UpdateFile  := "*** Update File: " path NEWLINE [ MoveTo ] { Hunk }
MoveTo      := "*** Move to: " newPath NEWLINE
Hunk        := "@@" [ " " header ] NEWLINE { HunkLine } [ "*** End of File" NEWLINE ]
HunkLine    := (" " | "-" | "+") text NEWLINE
```

## Context Rules
- By default, show 3 lines of unchanged code immediately above and below each change.
- Treat 3 lines as a minimum. Prefer 5-10 lines for large files, repeated code, or edits that could match in multiple places.
- Do not duplicate a previous change's context-after lines as the next change's context-before lines.
- If ordinary context is insufficient, use `@@ class_or_function` to narrow the location.
- Use multiple `@@` lines when one selector and ordinary context are still insufficient.

## Additional Rules
- When editing conflict markers, ensure their lengths match the existing markers.
- Every Add File content line must start with `+`.
- Update lines start with a space for context, `-` for removal, or `+` for addition.
- Use `*** End of File` to anchor a change at the end of a file.
- Multiple files can be patched in one call.
- Paths can be relative or absolute.
- Do not use apply_patch for changes an available formatter or linter can perform.

## Reliability Tips
- Use a unique `@@` selector and 5-10 context lines in repetitive files.
- If you read only part of a file, read more rather than guessing context.
- Preserve indentation exactly and avoid reindenting unrelated lines.
- Give insert-only hunks a selector or context instead of leaving their location ambiguous.
- Prefer longer, unique context over a short patch that can match multiple places.
- Copy internal whitespace exactly.
- Preserve the file's line-ending style.
- If `[REDACTED:_____]` appears and the patch fails, secret redaction may have changed the text. Ask the user to make the edit manually."#;

#[derive(Clone)]
pub struct ApplyPatchTool;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPatchInput {
    /// Complete Codex-format patch text.
    pub patch_text: String,
}

impl ToolDefinition for ApplyPatchTool {
    type Input = ApplyPatchInput;

    fn name(&self) -> &'static str {
        "apply_patch"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        let parsed = match parse_patch(&input.patch_text) {
            Ok(value) => value,
            Err(error) => return Ok(error_outcome(error, &[], &[])),
        };
        let warnings = parsed.warnings;
        let mut completed = Vec::new();
        let cwd = ctx.working_directory();
        for operation in parsed.operations {
            match apply_operation(&cwd, operation) {
                Ok(applied) => completed.push(applied),
                Err(error) => return Ok(error_outcome(error, &warnings, &completed)),
            }
        }
        let body = render_result(&warnings, &completed);
        Ok(ToolOutcome {
            content: vec![UserContent::text(body.clone())],
            details: ToolDetails::Text {
                summary: "Applied patch".into(),
                body,
            },
            is_error: false,
        })
    }
}

#[derive(Debug)]
enum Operation {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        destination: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug)]
struct Hunk {
    selector: Vec<String>,
    lines: Vec<EditLine>,
    eof: bool,
}

#[derive(Debug)]
enum EditLine {
    Context(String),
    Remove(String),
    Add(String),
}

struct Parsed {
    operations: Vec<Operation>,
    warnings: Vec<String>,
}

struct Applied {
    summary: String,
    diff: String,
}

fn unwrap_heredoc(text: &str) -> &str {
    let trimmed = text.trim();
    let mut lines = trimmed.lines();
    let Some(first) = lines.next() else {
        return trimmed;
    };
    if let Some(marker) = first.strip_prefix("<<") {
        let marker = marker.trim().trim_matches('\'').trim_matches('"');
        if !marker.is_empty() && trimmed.lines().last() == Some(marker) {
            let start = trimmed.find('\n').map_or(trimmed.len(), |n| n + 1);
            let end = trimmed.rfind('\n').unwrap_or(trimmed.len());
            return &trimmed[start..end];
        }
    }
    trimmed
}

fn parse_patch(raw: &str) -> Result<Parsed, String> {
    let text = unwrap_heredoc(raw);
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let begins: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| (*l == "*** Begin Patch").then_some(i))
        .collect();
    let Some(&begin) = begins.first() else {
        return Err("Invalid patch: missing `*** Begin Patch` envelope".into());
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(begin + 1)
        .find_map(|(i, l)| (*l == "*** End Patch").then_some(i))
        .ok_or("Invalid patch: missing `*** End Patch` envelope")?;
    let mut warnings = Vec::new();
    if lines[..begin].iter().any(|line| !line.trim().is_empty()) {
        warnings.push("Warning: ignored non-empty text before `*** Begin Patch`".into());
    }
    if lines[end + 1..].iter().any(|line| !line.trim().is_empty()) {
        warnings.push("Warning: ignored non-empty text after `*** End Patch`".into());
    }
    if begins.len() > 1 {
        warnings.push("Warning: ignored duplicate `*** Begin Patch` marker".into());
    }
    let mut operations = Vec::new();
    let mut i = begin + 1;
    while i < end {
        if lines[i].trim().is_empty() || lines[i] == "*** Begin Patch" {
            i += 1;
            continue;
        }
        if let Some(path) = lines[i].strip_prefix("*** Add File: ") {
            i += 1;
            let mut added = Vec::new();
            while i < end && !lines[i].starts_with("*** ") {
                let line = lines[i].strip_prefix('+').ok_or_else(|| {
                    format!("Malformed add for '{path}': every content line must start with `+`")
                })?;
                added.push(line);
                i += 1;
            }
            let content = if added.is_empty() {
                String::new()
            } else {
                added.join("\n") + "\n"
            };
            operations.push(Operation::Add {
                path: path.into(),
                content,
            });
        } else if let Some(path) = lines[i].strip_prefix("*** Delete File: ") {
            operations.push(Operation::Delete { path: path.into() });
            i += 1;
        } else if let Some(path) = lines[i].strip_prefix("*** Update File: ") {
            i += 1;
            let mut destination = None;
            if i < end {
                if let Some(to) = lines[i].strip_prefix("*** Move to: ") {
                    destination = Some(to.into());
                    i += 1;
                }
            }
            let mut hunks = Vec::new();
            while i < end && !is_operation_header(lines[i]) {
                if lines[i].trim().is_empty() {
                    i += 1;
                    continue;
                }
                let mut selector = Vec::new();
                while i < end && lines[i].starts_with("@@") {
                    selector.push(lines[i].trim_start_matches('@').trim().to_string());
                    i += 1;
                }
                if i >= end || is_operation_header(lines[i]) {
                    return Err(format!("Empty hunk in update for '{path}'"));
                }
                let mut edits = Vec::new();
                let mut eof = false;
                while i < end && !lines[i].starts_with("@@") && !is_operation_header(lines[i]) {
                    if lines[i] == "*** End of File" {
                        eof = true;
                        i += 1;
                        break;
                    }
                    let line = lines[i];
                    edits.push(if let Some(value) = line.strip_prefix(' ') {
                        EditLine::Context(value.into())
                    } else if let Some(value) = line.strip_prefix('-') {
                        EditLine::Remove(value.into())
                    } else if let Some(value) = line.strip_prefix('+') {
                        EditLine::Add(value.into())
                    } else {
                        return Err(format!("Malformed update line in '{path}': lines must start with space, `-`, or `+`"));
                    });
                    i += 1;
                }
                if edits.is_empty() {
                    return Err(format!("Empty hunk in update for '{path}'"));
                }
                hunks.push(Hunk {
                    selector,
                    lines: edits,
                    eof,
                });
            }
            if hunks.is_empty() {
                return Err(format!(
                    "No-op update for '{path}': provide at least one non-empty hunk"
                ));
            }
            operations.push(Operation::Update {
                path: path.into(),
                destination,
                hunks,
            });
        } else {
            return Err(format!(
                "Invalid patch directive or content outside a file operation: '{}'",
                lines[i]
            ));
        }
    }
    if operations.is_empty() {
        return Err("Patch contains no operations; add an Add, Delete, or Update directive".into());
    }
    Ok(Parsed {
        operations,
        warnings,
    })
}

fn is_operation_header(line: &str) -> bool {
    line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() { p } else { cwd.join(p) }
}

fn apply_operation(cwd: &Path, operation: Operation) -> Result<Applied, String> {
    match operation {
        Operation::Add { path, content } => {
            let target = resolve(cwd, &path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("Failed to create parent directories for '{path}': {e}")
                })?;
            }
            let original = fs::read_to_string(&target).unwrap_or_default();
            let diff = wire_diff(&path, &original, &content);
            fs::write(&target, &content).map_err(|e| format!("Failed to add '{path}': {e}"))?;
            Ok(Applied {
                summary: format!("add: {path} (+{}/-{})", diff.added, diff.removed),
                diff: diff.fenced,
            })
        }
        Operation::Delete { path } => {
            let target = resolve(cwd, &path);
            let original = fs::read_to_string(&target)
                .map_err(|e| format!("Failed to read '{path}' before deletion: {e}"))?;
            let diff = wire_diff(&path, &original, "");
            fs::remove_file(&target).map_err(|e| format!("Failed to delete '{path}': {e}"))?;
            Ok(Applied {
                summary: format!("delete: {path} (+0/-{})", diff.removed),
                diff: diff.fenced,
            })
        }
        Operation::Update {
            path,
            destination,
            hunks,
        } => {
            let source = resolve(cwd, &path);
            let original = fs::read_to_string(&source)
                .map_err(|e| format!("Failed to read '{path}' as UTF-8: {e}"))?;
            let crlf = original.contains("\r\n");
            let final_newline = original.ends_with('\n');
            let normalized = original.replace("\r\n", "\n");
            let mut lines: Vec<String> = normalized.lines().map(str::to_string).collect();
            apply_hunks(&path, &mut lines, hunks)?;
            let mut output = lines.join(if crlf { "\r\n" } else { "\n" });
            if !output.is_empty() && final_newline {
                output.push_str(if crlf { "\r\n" } else { "\n" });
            }
            if output == original {
                return Err(format!(
                    "No-op update for '{path}': patch leaves the file unchanged"
                ));
            }
            let target = destination
                .as_ref()
                .map_or_else(|| source.clone(), |to| resolve(cwd, to));
            let aliases_source = target != source
                && matches!(
                    (source.canonicalize(), target.canonicalize()),
                    (Ok(source), Ok(target)) if source == target
                );
            if aliases_source {
                return Err(format!(
                    "Refusing to move '{path}' onto another path for the same file"
                ));
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create destination for '{path}': {e}"))?;
            }
            let display_path = destination.as_deref().unwrap_or(&path);
            let diff = wire_diff(display_path, &original, &output);
            let previous_destination = (target != source).then(|| fs::read(&target).ok()).flatten();
            fs::write(&target, &output)
                .map_err(|e| format!("Failed to write '{}': {e}", target.display()))?;
            if target != source {
                if let Err(e) = fs::remove_file(&source) {
                    let rollback_result = match previous_destination {
                        Some(previous) => fs::write(&target, previous),
                        None => fs::remove_file(&target),
                    };
                    let rollback = rollback_result
                        .err()
                        .map(|r| format!("; rollback also failed: {r}"))
                        .unwrap_or_default();
                    return Err(format!(
                        "Moved content but failed to remove '{path}': {e}{rollback}"
                    ));
                }
                Ok(Applied {
                    summary: format!(
                        "move: {} (+{}/-{})",
                        destination.unwrap(),
                        diff.added,
                        diff.removed
                    ),
                    diff: diff.fenced,
                })
            } else {
                Ok(Applied {
                    summary: format!("update: {path} (+{}/-{})", diff.added, diff.removed),
                    diff: diff.fenced,
                })
            }
        }
    }
}

fn apply_hunks(path: &str, file: &mut Vec<String>, hunks: Vec<Hunk>) -> Result<(), String> {
    let mut cursor = 0;
    for hunk in hunks {
        let mut selector_matched = false;
        for selector in hunk.selector.iter().filter(|selector| !selector.is_empty()) {
            let Some((pos, _)) = find_match(file, &[selector], cursor, false) else {
                return Err(format!(
                    "Could not find hunk selector '{selector}' in '{path}'"
                ));
            };
            cursor = pos + 1;
            selector_matched = true;
        }
        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                EditLine::Context(s) | EditLine::Remove(s) => Some(s.as_str()),
                EditLine::Add(_) => None,
            })
            .collect();
        let (start, tier) = if old.is_empty() {
            (if selector_matched { cursor } else { file.len() }, 0)
        } else {
            find_match(file, &old, cursor, hunk.eof)
                .ok_or_else(|| format!("Could not match hunk context in '{path}'"))?
        };
        let patch_indent = old.first().map(|s| leading(s)).unwrap_or(0);
        let actual_indent = file.get(start).map(|s| leading(s)).unwrap_or(0);
        let mut replacement = Vec::new();
        let mut source_index = 0;
        for line in &hunk.lines {
            match line {
                EditLine::Context(_) => {
                    replacement.push(file[start + source_index].clone());
                    source_index += 1;
                }
                EditLine::Remove(_) => source_index += 1,
                EditLine::Add(value) => replacement.push(if tier > 0 {
                    shift_indent(value, patch_indent, actual_indent)
                } else {
                    value.clone()
                }),
            }
        }
        let replacement_len = replacement.len();
        file.splice(start..start + old.len(), replacement);
        cursor = start + replacement_len;
    }
    Ok(())
}

fn find_match(
    file: &[String],
    expected: &[&str],
    cursor: usize,
    eof: bool,
) -> Option<(usize, usize)> {
    let last = file.len().checked_sub(expected.len())?;
    for tier in 0..5 {
        let first = if eof { last } else { cursor };
        if first > last {
            return None;
        }
        for start in first..=last {
            if expected
                .iter()
                .enumerate()
                .all(|(offset, line)| equivalent(&file[start + offset], line, tier))
            {
                return Some((start, tier));
            }
        }
    }
    None
}

fn leading(s: &str) -> usize {
    s.len() - s.trim_start_matches(|c| c == ' ' || c == '\t').len()
}
fn shift_indent(s: &str, from: usize, to: usize) -> String {
    if s.len() >= from {
        format!("{}{}", " ".repeat(to), &s[from..])
    } else {
        s.into()
    }
}
fn equivalent(a: &str, b: &str, tier: usize) -> bool {
    match tier {
        0 => a == b,
        1 => a.trim_end() == b.trim_end(),
        2 => a.trim() == b.trim(),
        3 => punctuation(a.trim()) == punctuation(b.trim()),
        _ => collapse(&punctuation(a.trim())) == collapse(&punctuation(b.trim())),
    }
}
fn punctuation(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '‘' | '’' => '\'',
            '“' | '”' => '"',
            '–' | '—' => '-',
            '\u{00a0}' => ' ',
            other => other,
        })
        .collect()
}
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_result(warnings: &[String], completed: &[Applied]) -> String {
    let mut sections = Vec::new();
    if !warnings.is_empty() {
        sections.push(warnings.join("\n"));
    }
    sections.push(
        completed
            .iter()
            .map(|item| item.summary.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let diff_len: usize = completed.iter().map(|item| item.diff.len()).sum();
    if diff_len <= 1_048_576 {
        let diffs = completed
            .iter()
            .filter(|item| !item.diff.is_empty())
            .map(|item| item.diff.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !diffs.is_empty() {
            sections.push(diffs);
        }
    }
    sections.join("\n\n")
}

fn error_outcome(error: String, warnings: &[String], completed: &[Applied]) -> ToolOutcome {
    let body = if completed.is_empty() {
        error
    } else {
        format!(
            "{}\n\nError: {}\nEarlier operations were retained.",
            render_result(warnings, completed),
            error
        )
    };
    ToolOutcome {
        content: vec![UserContent::text(body.clone())],
        details: ToolDetails::Text {
            summary: "Patch failed".into(),
            body,
        },
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;
    use tempfile::TempDir;

    async fn run(dir: &TempDir, patch: &str) -> ToolOutcome {
        let mut ctx = DummyToolContext {
            working_directory: dir.path().into(),
            ..Default::default()
        };
        ApplyPatchTool
            .execute(
                &mut ctx,
                ApplyPatchInput {
                    patch_text: patch.into(),
                },
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn add_update_delete_move_and_multiple_operations() {
        let d = TempDir::new().unwrap();
        let out = run(
            &d,
            "*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n*** End Patch",
        )
        .await;
        assert!(!out.is_error);
        let out = run(&d, "*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+three\n*** Move to: ignored\n*** End Patch").await;
        assert!(
            out.is_error,
            "Move directive must immediately follow update header"
        );
        let out = run(&d, "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n one\n-two\n+three\n*** Add File: c/d.txt\n+x\n*** Delete File: c/d.txt\n*** End Patch").await;
        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(d.path().join("b.txt")).unwrap(),
            "one\nthree\n"
        );
        assert!(!d.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn partial_application_and_errors() {
        let d = TempDir::new().unwrap();
        let out = run(&d, "*** Begin Patch\n*** Add File: kept\n+yes\n*** Update File: absent\n@@\n-no\n+yes\n*** End Patch").await;
        assert!(out.is_error);
        assert!(d.path().join("kept").exists());
        for bad in [
            "",
            "*** Begin Patch\n*** End Patch",
            "*** Begin Patch\n*** Add File: x\nbad\n*** End Patch",
            "*** Begin Patch\n*** Update File: x\n*** End Patch",
            "*** Begin Patch\n*** Delete File: kept\ninvalid body\n*** End Patch",
        ] {
            assert!(run(&d, bad).await.is_error);
        }
        assert!(d.path().join("kept").exists());
    }

    #[tokio::test]
    async fn matching_tiers_eof_crlf_and_paths() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("f.txt");
        fs::write(&p, "start\r\nsmart — quote\r\nlast\r\n").unwrap();
        let patch = format!(
            "noise\n*** Begin Patch\n*** Update File: {}\n@@ start\n smart -   quote\n-last\n+done\n*** End of File\n*** End Patch\ntail",
            p.display()
        );
        let out = run(&d, &patch).await;
        assert!(!out.is_error);
        assert_eq!(
            fs::read(&p).unwrap(),
            b"start\r\nsmart \xe2\x80\x94 quote\r\ndone\r\n"
        );
    }

    #[tokio::test]
    async fn accepts_implicit_hunks_and_contextual_insertions() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("f.txt"), "start\nend\n").unwrap();

        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: f.txt\n-start\n+first\n@@ end\n+inserted\n*** End Patch",
        )
        .await;

        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(d.path().join("f.txt")).unwrap(),
            "first\nend\ninserted\n"
        );
    }

    #[tokio::test]
    async fn selectors_narrow_sequentially_and_hunks_follow_inserted_content() {
        let d = TempDir::new().unwrap();
        fs::write(
            d.path().join("f.txt"),
            "mod first {\n    fn target() {\n        old();\n        next();\n    }\n}\n",
        )
        .unwrap();

        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: f.txt\n@@ mod first {\n@@ fn target() {\n         old();\n+        inserted();\n@@\n         next();\n+        after();\n*** End Patch",
        )
        .await;

        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(d.path().join("f.txt")).unwrap(),
            "mod first {\n    fn target() {\n        old();\n        inserted();\n        next();\n        after();\n    }\n}\n"
        );
    }

    #[tokio::test]
    async fn oversized_context_fails_and_removing_all_lines_writes_an_empty_file() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("empty.txt"), "").unwrap();
        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: empty.txt\n@@\n-missing\n*** End Patch",
        )
        .await;
        assert!(out.is_error);

        fs::write(d.path().join("one.txt"), "only\n").unwrap();
        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: one.txt\n@@\n-only\n*** End Patch",
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(fs::read(d.path().join("one.txt")).unwrap(), b"");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_move_through_a_symlink_to_the_source() {
        use std::os::unix::fs::symlink;

        let d = TempDir::new().unwrap();
        fs::write(d.path().join("source.txt"), "old\n").unwrap();
        symlink("source.txt", d.path().join("alias.txt")).unwrap();

        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: source.txt\n*** Move to: alias.txt\n@@\n-old\n+new\n*** End Patch",
        )
        .await;

        assert!(out.is_error);
        assert_eq!(
            fs::read_to_string(d.path().join("source.txt")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn schema_and_mode() {
        let schema = serde_json::to_value(schemars::schema_for!(ApplyPatchInput)).unwrap();
        let text = schema.to_string();
        assert!(text.contains("patchText"));
        assert!(!text.contains("patch_text"));
        assert_eq!(ApplyPatchTool.execution_mode(), ExecutionMode::Sequential);
    }
}
