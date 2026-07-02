//! Unified-diff line computation for `Diff`-flavoured tool results.
//!
//! Turns the `path` / `before` / `after` triple of
//! [`aj_agent::tool::ToolDetails::Diff`] into a list of plain-text
//! lines tagged with a semantic [`DiffLineKind`]. Styling is left to
//! the consuming frontend, which keeps this crate free of any TUI
//! backend while both frontends render the same diff shape.
//!
//! The rendering is intentionally simple: line-level diff with a few
//! lines of context around each hunk. Syntax-highlighted unified
//! diffs (a longer-term goal) can swap in here without touching the
//! consumers.

use similar::{ChangeTag, TextDiff};

/// Semantic role of one rendered diff line. Frontends map each kind
/// onto their own styling (red for removals, green for additions,
/// muted for the rest).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    /// A `--- a/<path>` / `+++ b/<path>` file header line.
    Header,
    /// An inserted line, rendered with a `+ ` sign prefix.
    Add,
    /// A deleted line, rendered with a `- ` sign prefix.
    Remove,
    /// An unchanged line inside a hunk's context window, rendered
    /// with a two-space sign prefix.
    Context,
    /// The `…` separator between hunks whose context windows don't
    /// touch.
    Separator,
}

/// Number of unchanged lines kept around each hunk so the change
/// stays anchored without scrolling the entire file.
const CONTEXT: usize = 3;

/// Compute the unified-diff lines between `before` and `after`.
///
/// Each returned line carries its sign prefix (`+ `, `- `, two
/// spaces) in the text, so consumers only add color. `path` renders
/// as a `--- a/<path>` / `+++ b/<path>` header pair. When `before`
/// is empty (a fresh-file write) the deleted side is omitted, and
/// when `after` is empty the inserted side is omitted. Hunks
/// separated by more than the context window are joined by an `…`
/// separator line.
pub fn unified_diff_lines(path: &str, before: &str, after: &str) -> Vec<(DiffLineKind, String)> {
    let mut lines = Vec::new();

    if !before.is_empty() {
        lines.push((DiffLineKind::Header, format!("--- a/{path}")));
    }
    if !after.is_empty() {
        lines.push((DiffLineKind::Header, format!("+++ b/{path}")));
    }

    let diff = TextDiff::from_lines(before, after);

    // Snapshot the change tags up-front so we can do range-of-context
    // lookups without re-borrowing the lifetime-fussy `TextDiff` for
    // nested closures.
    let tags: Vec<ChangeTag> = diff.iter_all_changes().map(|c| c.tag()).collect();

    let mut last_emitted_idx: Option<usize> = None;
    for (idx, change) in diff.iter_all_changes().enumerate() {
        // Skip equal lines that fall outside the context window around
        // the closest non-equal change.
        if matches!(change.tag(), ChangeTag::Equal) && !is_in_context(&tags, idx, CONTEXT) {
            continue;
        }

        // Insert a separator if we skipped a span. Mirrors `git
        // --no-color`'s `@@` hunk markers without the line-number
        // arithmetic we don't need yet.
        if let Some(last) = last_emitted_idx {
            if idx > last + 1 {
                lines.push((DiffLineKind::Separator, "…".to_string()));
            }
        }
        last_emitted_idx = Some(idx);

        let value = change.value().trim_end_matches('\n');
        let line = match change.tag() {
            ChangeTag::Delete => (DiffLineKind::Remove, format!("- {value}")),
            ChangeTag::Insert => (DiffLineKind::Add, format!("+ {value}")),
            ChangeTag::Equal => (DiffLineKind::Context, format!("  {value}")),
        };
        lines.push(line);
    }

    lines
}

/// True if any change within `context` lines on either side of `idx`
/// is a non-equal change (insert/delete). Used to drop equal lines
/// that fall outside every hunk's context window.
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

    fn texts(lines: &[(DiffLineKind, String)]) -> Vec<&str> {
        lines.iter().map(|(_, t)| t.as_str()).collect()
    }

    #[test]
    fn creation_diff_omits_the_minus_header() {
        let out = unified_diff_lines("foo.txt", "", "hello\nworld\n");
        assert_eq!(out[0], (DiffLineKind::Header, "+++ b/foo.txt".to_string()));
        assert!(!texts(&out).iter().any(|t| t.starts_with("--- a/")));
        assert!(out.contains(&(DiffLineKind::Add, "+ hello".to_string())));
        assert!(out.contains(&(DiffLineKind::Add, "+ world".to_string())));
    }

    #[test]
    fn deletion_diff_omits_the_plus_header() {
        let out = unified_diff_lines("foo.txt", "hello\n", "");
        assert_eq!(out[0], (DiffLineKind::Header, "--- a/foo.txt".to_string()));
        assert!(!texts(&out).iter().any(|t| t.starts_with("+++ b/")));
        assert!(out.contains(&(DiffLineKind::Remove, "- hello".to_string())));
    }

    #[test]
    fn modification_diff_renders_both_headers_and_context() {
        let out = unified_diff_lines("foo.txt", "alpha\nbeta\n", "alpha\ngamma\n");
        assert_eq!(out[0], (DiffLineKind::Header, "--- a/foo.txt".to_string()));
        assert_eq!(out[1], (DiffLineKind::Header, "+++ b/foo.txt".to_string()));
        assert!(out.contains(&(DiffLineKind::Context, "  alpha".to_string())));
        assert!(out.contains(&(DiffLineKind::Remove, "- beta".to_string())));
        assert!(out.contains(&(DiffLineKind::Add, "+ gamma".to_string())));
    }

    #[test]
    fn equal_lines_outside_the_context_window_are_dropped() {
        // 9 unchanged lines between the change and the tail: only the
        // 3 closest render, the rest disappear entirely (no separator
        // at the end of the file because nothing follows).
        let before = "changed\na\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let after = "CHANGED\na\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let out = unified_diff_lines("t.txt", before, after);
        let t = texts(&out);
        assert!(t.contains(&"- changed"));
        assert!(t.contains(&"+ CHANGED"));
        assert!(t.contains(&"  c"), "third context line kept: {t:?}");
        assert!(!t.contains(&"  d"), "fourth line dropped: {t:?}");
        assert!(
            !out.iter().any(|(k, _)| *k == DiffLineKind::Separator),
            "no separator when nothing follows the elided span: {t:?}",
        );
    }

    #[test]
    fn far_apart_hunks_are_joined_by_a_separator() {
        let before = "one\na\nb\nc\nd\ne\nf\ng\ntwo\n";
        let after = "ONE\na\nb\nc\nd\ne\nf\ng\nTWO\n";
        let out = unified_diff_lines("t.txt", before, after);
        let seps = out
            .iter()
            .filter(|(k, _)| *k == DiffLineKind::Separator)
            .count();
        assert_eq!(seps, 1, "{:?}", texts(&out));
        // The separator sits between the two hunks.
        let sep_idx = out
            .iter()
            .position(|(k, _)| *k == DiffLineKind::Separator)
            .expect("separator present");
        let one_idx = out
            .iter()
            .position(|(_, t)| t == "+ ONE")
            .expect("first hunk");
        let two_idx = out
            .iter()
            .position(|(_, t)| t == "+ TWO")
            .expect("second hunk");
        assert!(one_idx < sep_idx && sep_idx < two_idx);
    }

    #[test]
    fn adjacent_hunks_get_no_separator() {
        // Context windows overlap when hunks sit close together, so
        // every equal line in between renders and no span is skipped.
        let before = "one\na\ntwo\n";
        let after = "ONE\na\nTWO\n";
        let out = unified_diff_lines("t.txt", before, after);
        assert!(
            !out.iter().any(|(k, _)| *k == DiffLineKind::Separator),
            "{:?}",
            texts(&out),
        );
        assert!(texts(&out).contains(&"  a"));
    }

    #[test]
    fn identical_sides_render_only_headers() {
        let out = unified_diff_lines("t.txt", "same\n", "same\n");
        assert!(out.iter().all(|(k, _)| *k == DiffLineKind::Header));
    }
}
