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
- `*** Add File: <path>` - create a new file. Every following line must start with `+`.
- `*** Delete File: <path>` - remove an existing file. Nothing follows.
- `*** Update File: <path>` - patch an existing file (optionally with a rename via `*** Move to:`).

### Grammar

```
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
- By default, show **3 lines** of unchanged code immediately above and 3 lines immediately below each change.
- Treat 3 lines as a minimum, not a target. For large files, repeated code, or any edit that could plausibly match in multiple places, prefer **5-10 lines** of unchanged context on each side.
- If a change is within the chosen context window of a previous change, do NOT duplicate the first change's context-after lines in the second change's context-before lines.
- If 3 lines of context is insufficient to uniquely identify the location, use the `@@` operator to indicate the class or function the snippet belongs to.
- If a code block is repeated, use multiple `@@` statements to narrow the location.

## Additional Rules
- **When editing conflict markers**, ensure their length matches the file's existing marker length.
- For Add File: every content line MUST start with `+` (which gets stripped)
- For Update File hunks: lines start with ` ` (context), `-` (remove), or `+` (add)
- Use `*** End of File` marker to anchor changes at end of file
- Multiple files can be patched in a single call
- File paths can be relative or absolute
- Don't use apply patch for edits that an available linter or formatter could do based on the instructions in the users AGENTS.md file.

## Reliability Tips (Hard Cases)
- Repeated blocks (CSS vars, test mocks, large "god" files): include a *unique* `@@ ...` header, and add 5-10 or more context lines until the target is unique.
- If you only read part of a file, do not guess. Read more of the file and expand the context until the hunk can match only once.
- Indentation-sensitive files (Svelte/CSS/TS): keep indentation exactly as in the file (tabs vs spaces). Do not reindent unrelated lines.
- Insert-only hunks (no `-` lines): avoid unanchored insert-only hunks; include a nearby unchanged context line to show *where* to insert.
- Ambiguous matches are worse than verbose hunks. Prefer a longer patch over a shorter patch that could apply in multiple places.
- Whitespace drift: avoid changing internal spacing in context lines. Copy context lines from the file.
- CRLF files: keep line endings consistent with the file you're patching.
- If you see `[REDACTED:_____]` in your inputs and edits fail, secret redaction may have changed the text; ask the user to manually make the edit.

## Examples

Add a new file:
```
*** Begin Patch
*** Add File: path/to/new/file.ts
+const hello = 'world'
+export { hello }
*** End Patch
```

Update a line using surrounding context as the anchor:
```
*** Begin Patch
*** Update File: src/config.ts
@@
 const retries = 3
-const timeout = 1000
+const timeout = 2000
 export const enabled = true
*** End Patch
```

Update a nested structure with enough context to disambiguate the edit:
```
*** Begin Patch
*** Update File: src/services/user-service.ts
@@ class UserService
   async updateUser(id: string, data: UserData) {
     const user = await this.findById(id)
-    user.name = data.name
+    user.name = data.name?.trim() || user.name
+    user.updatedAt = new Date()
     await this.save(user)
     return user
   }
*** End Patch
```

In a repetitive file, use a selector and a larger unique context window:
```
*** Begin Patch
*** Update File: src/routes.ts
@@ function registerAdminRoutes
 router.get('/admin/users', listUsers)
 router.get('/admin/teams', listTeams)
-router.get('/admin/audit', oldAudit)
+router.get('/admin/audit', auditLog)
 router.get('/admin/settings', settings)
*** End Patch
```

Use multiple `@@` blocks to skip intervening code:
```
*** Begin Patch
*** Update File: src/config/settings.ts
@@
 const defaultConfig = {
   name: 'myapp',
@@
   logging: {
-    level: 'info',
+    level: 'debug',
     format: 'json',
   },
*** End Patch
```

Match the existing conflict-marker length when resolving a conflict:
```
*** Begin Patch
*** Update File: src/version.ts
@@
-<<<<<<< HEAD
-export const version = '1'
-=======
-export const version = '2'
->>>>>>> feature
+export const version = '2'
*** End Patch
```

Delete a file:
```
*** Begin Patch
*** Delete File: path/to/delete.ts
*** End Patch
```

Move a file while changing its contents:
```
*** Begin Patch
*** Update File: src/old-name.ts
*** Move to: src/new-name.ts
@@
-export function oldName() {
+export function newName() {
   return 'hello'
 }
*** End Patch
```"#;

#[derive(Clone)]
pub struct ApplyPatchTool;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPatchInput {
    /// The full patch text that describes all changes to be made.
    #[serde(alias = "patch")]
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
            Err(error) => {
                let error = if error.starts_with("patch rejected:")
                    || error.starts_with("apply_patch verification failed:")
                {
                    error
                } else {
                    format!("apply_patch verification failed: {error}")
                };
                return Ok(error_outcome(error, &[], &[]));
            }
        };
        let warnings = parsed.warnings;
        let mut completed = Vec::new();
        let mut paths = Vec::new();
        let cwd = ctx.working_directory();
        let operation_count = parsed.operations.len();
        for (index, operation) in parsed.operations.into_iter().enumerate() {
            let operation_label = operation.label();
            paths.push(operation.result_path().to_string());
            match apply_operation(&cwd, operation) {
                Ok(Some(applied)) => completed.push(applied),
                Ok(None) => {}
                Err(error) => {
                    let error = if operation_count == 1 {
                        error
                    } else {
                        format!(
                            "{error}\nFailed operation {}/{}: {}",
                            index + 1,
                            operation_count,
                            operation_label
                        )
                    };
                    return Ok(error_outcome(error, &warnings, &completed));
                }
            }
        }
        if completed.is_empty() {
            return Ok(error_outcome(
                format!(
                    "patch rejected: the patch produced no changes. The content you provided is identical to what is already in the file(s): {}. Read the file first to see its current contents, then provide a patch with actual changes.",
                    paths.join(", ")
                ),
                &warnings,
                &[],
            ));
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

impl Operation {
    fn result_path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } => path,
            Self::Update {
                path, destination, ..
            } => destination.as_deref().unwrap_or(path),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Add { path, .. } => format!("add {path}"),
            Self::Delete { path } => format!("delete {path}"),
            Self::Update { path, .. } => format!("update {path}"),
        }
    }
}

#[derive(Debug)]
struct Hunk {
    selector: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    old_line_index_for_new_line: Vec<Option<usize>>,
    eof: bool,
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
    let declaration = first.strip_prefix("cat ").unwrap_or(first);
    if let Some(marker) = declaration.strip_prefix("<<") {
        let marker = marker.trim().trim_matches('\'').trim_matches('"');
        if !marker.is_empty()
            && marker.chars().all(|c| c == '_' || c.is_alphanumeric())
            && trimmed.lines().last() == Some(marker)
        {
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
    let begin = lines
        .iter()
        .position(|line| line.trim() == "*** Begin Patch");
    let end = lines.iter().position(|line| line.trim() == "*** End Patch");
    let (Some(begin), Some(end)) = (begin, end) else {
        return Err(match (begin, end) {
            (None, None) => "Invalid patch format: missing *** Begin Patch and *** End Patch markers.\nExpected format:\n*** Begin Patch\n*** Add File: path/to/file.ts\n+file contents\n*** End Patch".into(),
            (None, Some(_)) => "Invalid patch format: missing *** Begin Patch marker. Patch must start with \"*** Begin Patch\"".into(),
            (Some(_), None) => "Invalid patch format: missing *** End Patch marker. Patch must end with \"*** End Patch\"".into(),
            _ => unreachable!(),
        });
    };
    if end < begin {
        return Err("Invalid patch format: *** End Patch appears before *** Begin Patch. Check marker ordering.".into());
    }
    let mut warnings = Vec::new();
    let before: Vec<&str> = lines[..begin]
        .iter()
        .copied()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if let Some(first) = before.first() {
        warnings.push(format!(
            "Warning: {} non-empty line(s) before *** Begin Patch were ignored. First ignored: \"{}\"",
            before.len(),
            truncate(first, 40)
        ));
    }
    if let Some(duplicate) = lines
        .iter()
        .enumerate()
        .skip(begin + 1)
        .take(end.saturating_sub(begin + 1))
        .find_map(|(i, line)| (line.trim() == "*** Begin Patch").then_some(i))
    {
        warnings.push(format!(
            "Warning: duplicate \"*** Begin Patch\" found at line {}. Only the first marker is used.",
            duplicate + 1
        ));
    }
    let after: Vec<&str> = lines[end + 1..]
        .iter()
        .copied()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if let Some(first) = after.first() {
        warnings.push(format!(
            "Warning: {} non-empty line(s) after *** End Patch were ignored. First ignored: \"{}\"",
            after.len(),
            truncate(first, 40)
        ));
    }
    let mut operations = Vec::new();
    let mut i = begin + 1;
    while i < end {
        if let Some(path) = lines[i].strip_prefix("*** Add File: ") {
            if path.is_empty() {
                i += 1;
                continue;
            }
            i += 1;
            let mut added = Vec::new();
            while i < end && !is_operation_header(lines[i]) {
                if let Some(line) = lines[i].strip_prefix('+') {
                    added.push(line);
                } else if lines[i].is_empty() {
                    added.push("");
                } else {
                    return Err(format!(
                        "Invalid patch format: Add File lines must start with '+', got: \"{}\"",
                        truncate(lines[i], 20)
                    ));
                }
                i += 1;
            }
            operations.push(Operation::Add {
                path: path.into(),
                content: added.join("\n"),
            });
        } else if let Some(path) = lines[i].strip_prefix("*** Delete File: ") {
            if !path.is_empty() {
                operations.push(Operation::Delete { path: path.into() });
            }
            i += 1;
        } else if let Some(path) = lines[i].strip_prefix("*** Update File: ") {
            if path.is_empty() {
                i += 1;
                continue;
            }
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
                if lines[i].trim() == "*** End of File" {
                    i += 1;
                    continue;
                }
                if !lines[i].starts_with("@@") && !is_edit_line(lines[i]) {
                    return Err(format!(
                        "Invalid patch format: unexpected line in Update File: \"{}\"",
                        truncate(lines[i], 30)
                    ));
                }
                let mut selectors = Vec::new();
                while i < end && lines[i].starts_with("@@") {
                    let selector = lines[i][2..].trim_start();
                    if !selector.is_empty() {
                        selectors.push(selector.to_string());
                    }
                    i += 1;
                }
                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut old_line_index_for_new_line = Vec::new();
                let mut removed_indices = Vec::new();
                let mut addition_index = 0;
                let mut eof = false;
                while i < end && !lines[i].starts_with("@@") && !is_operation_header(lines[i]) {
                    if lines[i].trim() == "*** End of File" {
                        eof = true;
                        i += 1;
                        break;
                    }
                    let line = lines[i];
                    if let Some(value) = line.strip_prefix(' ') {
                        removed_indices.clear();
                        addition_index = 0;
                        old_line_index_for_new_line.push(Some(old_lines.len()));
                        old_lines.push(value.into());
                        new_lines.push(value.into());
                    } else if let Some(value) = line.strip_prefix('-') {
                        removed_indices.push(old_lines.len());
                        old_lines.push(value.into());
                    } else if let Some(value) = line.strip_prefix('+') {
                        old_line_index_for_new_line
                            .push(removed_indices.get(addition_index).copied());
                        addition_index += 1;
                        new_lines.push(value.into());
                    } else {
                        return Err(format!(
                            "Invalid patch format: hunk lines must start with ' ', '-', or '+', got: \"{}\"",
                            truncate(line, 20)
                        ));
                    }
                    i += 1;
                }
                hunks.push(Hunk {
                    selector: (!selectors.is_empty()).then(|| selectors.join("\n")),
                    old_lines,
                    new_lines,
                    old_line_index_for_new_line,
                    eof,
                });
            }
            operations.push(Operation::Update {
                path: path.into(),
                destination,
                hunks,
            });
        } else {
            i += 1;
        }
    }
    if operations.is_empty() {
        return Err(if text.trim() == "*** Begin Patch\n*** End Patch" {
            "patch rejected: empty patch body. You sent a patch with no file operations between \"*** Begin Patch\" and \"*** End Patch\". Include at least one file operation (e.g., \"*** Update File: path/to/file\").".into()
        } else {
            "apply_patch verification failed: no hunks found. The patch text could not be parsed into any file operations. Ensure the patch follows the correct format with \"*** Begin Patch\", file operations like \"*** Update File: path/to/file\", and \"*** End Patch\".".into()
        });
    }
    Ok(Parsed {
        operations,
        warnings,
    })
}

fn is_operation_header(line: &str) -> bool {
    line.starts_with("*** Add File:")
        || line.starts_with("*** Delete File:")
        || line.starts_with("*** Update File:")
        || line.trim() == "*** End Patch"
}

fn is_edit_line(line: &str) -> bool {
    line.starts_with([' ', '-', '+'])
}

fn truncate(value: &str, length: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(length).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() { p } else { cwd.join(p) }
}

fn apply_operation(cwd: &Path, operation: Operation) -> Result<Option<Applied>, String> {
    match operation {
        Operation::Add { path, mut content } => {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            let diff = wire_diff(&path, "", &content);
            if diff.added == 0 && diff.removed == 0 {
                return Ok(None);
            }
            let target = resolve(cwd, &path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("Failed to create parent directories for '{path}': {e}")
                })?;
            }
            fs::write(&target, &content).map_err(|e| format!("Failed to add '{path}': {e}"))?;
            Ok(Some(Applied {
                summary: format!("add: {path} (+{}/-{})", diff.added, diff.removed),
                diff: diff.fenced,
            }))
        }
        Operation::Delete { path } => {
            let target = resolve(cwd, &path);
            let original_bytes = fs::read(&target).map_err(|_| {
                "file not found. Cannot delete a file that doesn't exist.".to_string()
            })?;
            let original = String::from_utf8_lossy(&original_bytes);
            let removed = original.split('\n').count();
            let diff = wire_diff(&path, &original, "");
            fs::remove_file(&target).map_err(|e| format!("Failed to delete '{path}': {e}"))?;
            Ok(Some(Applied {
                summary: format!("delete: {path} (+0/-{removed})"),
                diff: diff.fenced,
            }))
        }
        Operation::Update {
            path,
            destination,
            hunks,
        } => {
            let source = resolve(cwd, &path);
            let original_bytes = fs::read(&source).map_err(|_| {
                "file not found. Cannot update a file that doesn't exist.".to_string()
            })?;
            let original = String::from_utf8_lossy(&original_bytes).into_owned();
            let crlf = original.contains("\r\n");
            let normalized = original.replace("\r\n", "\n");
            let mut lines: Vec<String> = normalized.lines().map(str::to_string).collect();
            apply_hunks(&path, &mut lines, hunks)?;
            if lines.is_empty() {
                lines.extend([String::new(), String::new()]);
            } else if lines.last().is_some_and(|line| !line.is_empty()) {
                lines.push(String::new());
            }
            let output = lines.join(if crlf { "\r\n" } else { "\n" });
            if output == original {
                return Ok(None);
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
                Ok(Some(Applied {
                    summary: format!(
                        "move: {} (+{}/-{})",
                        destination.unwrap(),
                        diff.added,
                        diff.removed
                    ),
                    diff: diff.fenced,
                }))
            } else {
                Ok(Some(Applied {
                    summary: format!("update: {path} (+{}/-{})", diff.added, diff.removed),
                    diff: diff.fenced,
                }))
            }
        }
    }
}

fn apply_hunks(path: &str, file: &mut Vec<String>, hunks: Vec<Hunk>) -> Result<(), String> {
    struct Replacement {
        start: usize,
        delete_count: usize,
        insert: Vec<String>,
    }

    let mut cursor = 0;
    let mut replacements: Vec<Replacement> = Vec::new();
    for mut hunk in hunks {
        let selector_matched = hunk.selector.as_ref().is_some_and(|selector| {
            let selector_lines: Vec<&str> = selector.lines().collect();
            if let Some((position, _)) = find_match(file, &selector_lines, cursor, false) {
                cursor = position + selector_lines.len();
                true
            } else {
                false
            }
        });
        if hunk.old_lines.is_empty() {
            let start = if selector_matched { cursor } else { file.len() };
            if let Some(previous) = replacements.last_mut() {
                if previous.start == start && previous.delete_count == 0 {
                    previous.insert.append(&mut hunk.new_lines);
                    continue;
                }
            }
            replacements.push(Replacement {
                start,
                delete_count: 0,
                insert: hunk.new_lines,
            });
            continue;
        }
        let mut old: Vec<&str> = hunk.old_lines.iter().map(String::as_str).collect();
        let mut matched = find_match(file, &old, cursor, hunk.eof);
        if matched.is_none() && hunk.old_lines.last().is_some_and(String::is_empty) {
            hunk.old_lines.pop();
            if hunk.new_lines.last().is_some_and(String::is_empty) {
                hunk.new_lines.pop();
                hunk.old_line_index_for_new_line.pop();
            }
            old = hunk.old_lines.iter().map(String::as_str).collect();
            matched = find_match(file, &old, cursor, hunk.eof);
        }
        let (start, tier) = matched.ok_or_else(|| {
            let location = hunk.selector.as_ref().map_or_else(
                || format!("in '{path}'"),
                |selector| format!("near \"{selector}\" in '{path}'"),
            );
            format!(
                "Could not find matching lines {location}.\nExpected to find:\n{}{}",
                expected_lines(&hunk.old_lines),
                candidate_mismatch(file, &hunk.old_lines, cursor)
            )
        })?;
        let actual = &file[start..start + old.len()];
        let insert = adapt_replacement(
            actual,
            &hunk.old_lines,
            &hunk.new_lines,
            &hunk.old_line_index_for_new_line,
            tier,
        );
        replacements.push(Replacement {
            start,
            delete_count: old.len(),
            insert,
        });
        cursor = start + old.len();
    }
    replacements.sort_by_key(|replacement| replacement.start);
    for pair in replacements.windows(2) {
        let previous_end = pair[0].start + pair[0].delete_count;
        if pair[1].start < previous_end {
            return Err(format!(
                "Overlapping patch chunks in {path}: replacement starting at line {} overlaps previous replacement ending at line {previous_end}.",
                pair[1].start + 1
            ));
        }
    }
    for replacement in replacements.into_iter().rev() {
        file.splice(
            replacement.start..replacement.start + replacement.delete_count,
            replacement.insert,
        );
    }
    Ok(())
}

fn expected_lines(lines: &[String]) -> String {
    let mut expected = lines
        .iter()
        .take(3)
        .map(|line| format!("  {line:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > 3 {
        expected.push_str("\n  ...");
    }
    expected
}

fn candidate_mismatch(file: &[String], expected: &[String], cursor: usize) -> String {
    let Some(first) = expected.first() else {
        return String::new();
    };
    let candidate = file
        .iter()
        .enumerate()
        .skip(cursor.min(file.len()))
        .max_by_key(|(_, line)| {
            let tier_score = (0..5)
                .rev()
                .find(|tier| equivalent(line, first, *tier))
                .map_or(0, |tier| 5 - tier);
            let prefix_score = line
                .chars()
                .zip(first.chars())
                .take_while(|(actual, expected)| actual == expected)
                .count();
            tier_score * 1_000 + prefix_score
        });
    match candidate {
        Some((line, actual)) => format!(
            "\nClosest candidate mismatch at line {}:\n  expected {:?}\n  actual   {:?}",
            line + 1,
            first,
            actual
        ),
        None => "\nNo candidate lines remain after the hunk anchor.".into(),
    }
}

fn adapt_replacement(
    actual: &[String],
    old: &[String],
    new: &[String],
    old_indices: &[Option<usize>],
    tier: usize,
) -> Vec<String> {
    if tier == 0 {
        return new.to_vec();
    }
    let mut replacement = new.to_vec();
    for (index, value) in new.iter().enumerate() {
        let Some(old_index) = old_indices.get(index).copied().flatten() else {
            continue;
        };
        let (Some(actual), Some(expected)) = (actual.get(old_index), old.get(old_index)) else {
            continue;
        };
        if expected.trim() == value.trim() {
            replacement[index] = actual.clone();
        } else if indentation(expected) == indentation(value) {
            replacement[index] = format!("{}{}", indentation(actual), value.trim_start());
        }
    }
    replacement
}

fn find_match(
    file: &[String],
    expected: &[&str],
    cursor: usize,
    eof: bool,
) -> Option<(usize, usize)> {
    if expected.is_empty() {
        return None;
    }
    let last = file.len().checked_sub(expected.len())?;
    for tier in 0..5 {
        if cursor > last {
            return None;
        }
        if eof
            && expected
                .iter()
                .enumerate()
                .all(|(offset, line)| equivalent(&file[last + offset], line, tier))
        {
            return Some((last, tier));
        }
        for start in cursor..=last {
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

fn indentation(value: &str) -> &str {
    &value[..value.len() - value.trim_start_matches([' ', '\t']).len()]
}

fn equivalent(a: &str, b: &str, tier: usize) -> bool {
    match tier {
        0 => a == b,
        1 => a.trim_end() == b.trim_end(),
        2 => a.trim() == b.trim(),
        3 => punctuation(a.trim()) == punctuation(b.trim()),
        _ => collapse(&punctuation(a)) == collapse(&punctuation(b)),
    }
}
fn punctuation(s: &str) -> String {
    let mut normalized = String::with_capacity(s.len());
    for character in s.chars() {
        match character {
            '‘' | '’' => normalized.push('\''),
            '“' | '”' => normalized.push('"'),
            '‐' | '‑' | '‒' | '–' | '—' | '―' => normalized.push('-'),
            '…' => normalized.push_str("..."),
            '\u{00a0}' => normalized.push(' '),
            other => normalized.push(other),
        }
    }
    normalized
}
fn collapse(s: &str) -> String {
    s.replace('\t', " ")
        .trim()
        .split(' ')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
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
            "apply_patch partially applied {} operation(s), then failed:\n{}\n\n{}",
            completed.len(),
            error,
            render_result(warnings, completed)
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

    fn body(outcome: &ToolOutcome) -> &str {
        let UserContent::Text(body) = &outcome.content[0] else {
            panic!("expected text output");
        };
        &body.text
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
        assert!(body(&out).starts_with(
            "apply_patch partially applied 1 operation(s), then failed:\nfile not found. Cannot update a file that doesn't exist."
        ));
        assert!(body(&out).contains("Failed operation 2/2: update absent"));
        assert!(body(&out).contains("add: kept (+1/-0)"));
        assert!(body(&out).contains("```diff"));
        for bad in [
            "",
            "*** Begin Patch\n*** End Patch",
            "*** Begin Patch\n*** Add File: x\nbad\n*** End Patch",
            "*** Begin Patch\n*** Update File: x\ninvalid body\n*** End Patch",
        ] {
            assert!(run(&d, bad).await.is_error);
        }
        assert!(d.path().join("kept").exists());

        let out = run(
            &d,
            "*** Begin Patch\n*** Delete File: kept\nignored body\n*** End Patch",
        )
        .await;
        assert!(!out.is_error);
        assert!(!d.path().join("kept").exists());
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
    async fn adjacent_selectors_and_multiple_hunks_apply_against_original_content() {
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
    async fn oversized_context_fails_and_removing_all_lines_leaves_a_newline() {
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
        assert_eq!(fs::read(d.path().join("one.txt")).unwrap(), b"\n");
    }

    #[tokio::test]
    async fn update_preserves_planned_trailing_blank_semantics() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("trailing.txt"), "old\n").unwrap();
        fs::write(d.path().join("blank.txt"), "old\n").unwrap();

        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: trailing.txt\n@@\n-old\n+a\n+\n*** Update File: blank.txt\n@@\n-old\n+\n*** End Patch",
        )
        .await;

        assert!(!out.is_error, "{}", body(&out));
        assert_eq!(fs::read(d.path().join("trailing.txt")).unwrap(), b"a\n");
        assert_eq!(fs::read(d.path().join("blank.txt")).unwrap(), b"");
    }

    #[tokio::test]
    async fn delete_decodes_lossily_and_counts_split_lines() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("binary"), [0xff, b'\n']).unwrap();
        fs::write(d.path().join("empty"), []).unwrap();
        fs::write(d.path().join("trailing"), b"a\n").unwrap();

        let out = run(
            &d,
            "*** Begin Patch\n*** Delete File: binary\n*** Delete File: empty\n*** Delete File: trailing\n*** End Patch",
        )
        .await;

        assert!(!out.is_error, "{}", body(&out));
        assert!(!d.path().join("binary").exists());
        assert!(body(&out).contains("delete: binary (+0/-2)"));
        assert!(body(&out).contains("delete: empty (+0/-1)"));
        assert!(body(&out).contains("delete: trailing (+0/-2)"));
    }

    #[tokio::test]
    async fn parser_errors_have_one_verification_prefix() {
        let d = TempDir::new().unwrap();

        let malformed = run(&d, "not a patch").await;
        assert!(body(&malformed).starts_with(
            "apply_patch verification failed: Invalid patch format: missing *** Begin Patch"
        ));

        let empty = run(&d, "*** Begin Patch\n*** End Patch").await;
        assert!(body(&empty).starts_with("patch rejected: empty patch body."));
        assert!(!body(&empty).contains("verification failed"));

        let no_operations = run(&d, "*** Begin Patch\nignored\n*** End Patch").await;
        assert!(
            body(&no_operations).starts_with("apply_patch verification failed: no hunks found.")
        );
        assert_eq!(
            body(&no_operations)
                .matches("apply_patch verification failed:")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn failed_hunk_reports_the_closest_candidate_line() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("f.txt"), "zero\nalpha original\ntail\n").unwrap();

        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-alpha expected\n+replacement\n*** End Patch",
        )
        .await;

        assert!(out.is_error);
        assert!(body(&out).contains("Closest candidate mismatch at line 2:"));
        assert!(body(&out).contains("expected \"alpha expected\""));
        assert!(body(&out).contains("actual   \"alpha original\""));
    }

    #[tokio::test]
    async fn accepts_amp_envelope_heredoc_and_top_level_tolerance() {
        let d = TempDir::new().unwrap();
        let out = run(
            &d,
            "cat <<'PATCH'\nignored before\n  *** Begin Patch  \nignored inside\n*** Add File: blank.txt\n+first\n\n+third\n  *** End Patch  \nignored after\nPATCH",
        )
        .await;

        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(d.path().join("blank.txt")).unwrap(),
            "first\n\nthird\n"
        );
    }

    #[tokio::test]
    async fn missing_and_noncontiguous_selectors_are_advisory() {
        let d = TempDir::new().unwrap();
        fs::write(
            d.path().join("f.txt"),
            "class Outer\nintervening\nfunction inner\nold\n",
        )
        .unwrap();
        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: f.txt\n@@ missing\n-old\n+new\n@@ class Outer\n@@ function inner\n+appended\n*** End Patch",
        )
        .await;

        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(d.path().join("f.txt")).unwrap(),
            "class Outer\nintervening\nfunction inner\nnew\nappended\n"
        );
    }

    #[tokio::test]
    async fn eof_is_preferred_but_falls_back_and_fuzzy_indent_is_preserved() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("f.txt"), "    old\ntail\n").unwrap();
        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: f.txt\n-old\n+replacement\n*** End of File\n*** End Patch",
        )
        .await;

        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(d.path().join("f.txt")).unwrap(),
            "    replacement\ntail\n"
        );
    }

    #[tokio::test]
    async fn add_overwrites_as_new_and_no_op_updates_are_skipped() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("existing.txt"), "old\n").unwrap();
        fs::write(d.path().join("same.txt"), "same\n").unwrap();
        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: same.txt\n same\n*** Add File: existing.txt\n+new\n*** End Patch",
        )
        .await;

        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(d.path().join("existing.txt")).unwrap(),
            "new\n"
        );
        let UserContent::Text(body) = &out.content[0] else {
            panic!("expected text output");
        };
        let body = &body.text;
        assert!(body.contains("add: existing.txt (+1/-0)"));

        let out = run(
            &d,
            "*** Begin Patch\n*** Update File: same.txt\n same\n*** End Patch",
        )
        .await;
        assert!(out.is_error);
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
