//! Diff rendering for the `Diff`-flavoured tool result.
//!
//! Builds a unified-diff line list from the `before` / `after` byte
//! pair the tool surfaced (see
//! [`aj_agent::tool::ToolDetails::Diff`]) and styles it with the
//! shared [`aj_tui::style`] palette. Used by
//! [`super::tool_execution::ToolExecutionComponent`] when the
//! current tool call carries a `Diff` payload — `write_file`,
//! `edit_file`, `edit_file_multi`.
//!
//! The rendering is intentionally simple: line-level diff with a
//! few lines of context around each hunk, +/- prefixed with
//! red/green colour. Syntax-highlighted unified diffs (a longer-term
//! goal) can swap in here without touching the surrounding component.

use aj_tui::style;
use similar::{ChangeTag, TextDiff};

/// Render a unified diff between `before` and `after` to a list of
/// styled lines. Each line is already padded with the matching
/// sign (`+`, `-`, ` `) and carries inline ANSI escapes for
/// red (`-`) / green (`+`) / dim (` `) colouring.
///
/// `path` is rendered above the hunks as a `--- a/<path>` /
/// `+++ b/<path>` pair so the consumer can see at a glance which
/// file the diff applies to. When `before` is empty (a fresh-file
/// `write_file`) the deleted side is omitted; when `after` is
/// empty (a hypothetical future `delete_file`) the inserted side
/// is omitted.
///
/// Lines around each change keep a small (3-line) context window
/// so the user can see what the change is anchored to without
/// having to scroll the entire file. Hunks separated by more than
/// the context window are joined by an `…` separator.
pub fn render_unified_diff(path: &str, before: &str, after: &str) -> Vec<String> {
    let mut lines = Vec::new();

    // Header: `--- a/<path>` / `+++ b/<path>`. The dim style
    // mirrors what most pagers do, so the eye gravitates to the
    // change rows rather than the framing.
    if !before.is_empty() {
        lines.push(style::dim(&format!("--- a/{path}")));
    }
    if !after.is_empty() {
        lines.push(style::dim(&format!("+++ b/{path}")));
    }

    // Build the diff once so we can iterate hunks cheaply.
    let diff = TextDiff::from_lines(before, after);
    const CONTEXT: usize = 3;

    // Snapshot the change tags up-front so we can do
    // range-of-context lookups without re-borrowing the
    // lifetime-fussy `TextDiff` for nested closures.
    let tags: Vec<ChangeTag> = diff.iter_all_changes().map(|c| c.tag()).collect();

    let mut last_emitted_idx: Option<usize> = None;
    for (idx, change) in diff.iter_all_changes().enumerate() {
        // Skip equal lines that fall outside the context window
        // around the closest non-equal change.
        if matches!(change.tag(), ChangeTag::Equal) && !is_in_context(&tags, idx, CONTEXT) {
            continue;
        }

        // Insert a separator if we skipped a span. The separator
        // is a dim ellipsis on its own line; mirrors `git
        // --no-color`'s `@@` hunk markers without the
        // line-number arithmetic we don't need yet.
        if let Some(last) = last_emitted_idx {
            if idx > last + 1 {
                lines.push(style::dim("…"));
            }
        }
        last_emitted_idx = Some(idx);

        let value = change.value().trim_end_matches('\n');
        let styled = match change.tag() {
            ChangeTag::Delete => style::red(&format!("- {value}")),
            ChangeTag::Insert => style::green(&format!("+ {value}")),
            ChangeTag::Equal => style::dim(&format!("  {value}")),
        };
        lines.push(styled);
    }

    lines
}

/// True if any change within `context` lines on either side of
/// `idx` is a non-equal change (insert/delete). Used by
/// [`render_unified_diff`] to drop equal lines that fall outside
/// every hunk's context window.
fn is_in_context(tags: &[ChangeTag], idx: usize, context: usize) -> bool {
    let lo = idx.saturating_sub(context);
    let hi = idx
        .saturating_add(context)
        .min(tags.len().saturating_sub(1));
    (lo..=hi).any(|i| !matches!(tags[i], ChangeTag::Equal))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        let mut out: Vec<u8> = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // Skip until terminator (`m` for SGR).
                i += 2;
                while i < bytes.len() && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        // Source is &str (valid UTF-8) and we only ever skipped
        // ASCII SGR sequences, so the survivors are still valid
        // UTF-8.
        String::from_utf8(out).expect("strip_ansi: surviving bytes remain valid UTF-8")
    }

    #[test]
    fn renders_a_creation_diff_without_a_minus_header() {
        let out = render_unified_diff("foo.txt", "", "hello\nworld\n");
        let plain: Vec<_> = out.iter().map(|s| strip_ansi(s)).collect();
        assert_eq!(plain[0], "+++ b/foo.txt");
        assert!(plain.iter().any(|l| l == "+ hello"));
        assert!(plain.iter().any(|l| l == "+ world"));
    }

    #[test]
    fn renders_a_modification_diff_with_both_headers() {
        let out = render_unified_diff("foo.txt", "alpha\nbeta\n", "alpha\ngamma\n");
        let plain: Vec<_> = out.iter().map(|s| strip_ansi(s)).collect();
        assert_eq!(plain[0], "--- a/foo.txt");
        assert_eq!(plain[1], "+++ b/foo.txt");
        // Equal context line.
        assert!(plain.iter().any(|l| l == "  alpha"));
        // The actual change.
        assert!(plain.iter().any(|l| l == "- beta"));
        assert!(plain.iter().any(|l| l == "+ gamma"));
    }
}
