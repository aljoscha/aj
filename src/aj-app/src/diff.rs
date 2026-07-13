//! Compatibility helpers for canonical file-edit display diffs.

use aj_agent::tool::DiffDetails;
pub use aj_agent::tool::DiffLineKind;

/// Computes canonical display lines from file snapshots.
///
/// New code should construct [`DiffDetails`] once at the tool boundary and
/// consume [`DiffDetails::lines`] directly. This helper remains for callers
/// that still hold snapshots.
pub fn unified_diff_lines(path: &str, before: &str, after: &str) -> Vec<(DiffLineKind, String)> {
    DiffDetails::new(path, before, after)
        .lines()
        .iter()
        .map(|line| (line.kind(), line.text().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[(DiffLineKind, String)]) -> Vec<&str> {
        lines.iter().map(|(_, text)| text.as_str()).collect()
    }

    #[test]
    fn creation_diff_omits_the_minus_header() {
        let out = unified_diff_lines("foo.txt", "", "hello\nworld\n");
        assert_eq!(out[0], (DiffLineKind::Header, "+++ b/foo.txt".to_string()));
        assert!(!texts(&out).iter().any(|text| text.starts_with("--- a/")));
        assert!(out.contains(&(DiffLineKind::Add, "+ hello".to_string())));
        assert!(out.contains(&(DiffLineKind::Add, "+ world".to_string())));
    }

    #[test]
    fn deletion_diff_omits_the_plus_header() {
        let out = unified_diff_lines("foo.txt", "hello\n", "");
        assert_eq!(out[0], (DiffLineKind::Header, "--- a/foo.txt".to_string()));
        assert!(!texts(&out).iter().any(|text| text.starts_with("+++ b/")));
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
        let before = "changed\na\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let after = "CHANGED\na\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let out = unified_diff_lines("t.txt", before, after);
        let text = texts(&out);
        assert!(text.contains(&"- changed"));
        assert!(text.contains(&"+ CHANGED"));
        assert!(text.contains(&"  c"), "third context line kept: {text:?}");
        assert!(!text.contains(&"  d"), "fourth line dropped: {text:?}");
        assert!(
            !out.iter().any(|(kind, _)| *kind == DiffLineKind::Separator),
            "no trailing separator: {text:?}",
        );
    }

    #[test]
    fn far_apart_hunks_are_joined_by_a_separator() {
        let before = "one\na\nb\nc\nd\ne\nf\ng\ntwo\n";
        let after = "ONE\na\nb\nc\nd\ne\nf\ng\nTWO\n";
        let out = unified_diff_lines("t.txt", before, after);
        let separators = out
            .iter()
            .filter(|(kind, _)| *kind == DiffLineKind::Separator)
            .count();
        assert_eq!(separators, 1, "{:?}", texts(&out));
        let separator = out
            .iter()
            .position(|(kind, _)| *kind == DiffLineKind::Separator)
            .expect("separator present");
        let one = out
            .iter()
            .position(|(_, text)| text == "+ ONE")
            .expect("first hunk");
        let two = out
            .iter()
            .position(|(_, text)| text == "+ TWO")
            .expect("second hunk");
        assert!(one < separator && separator < two);
    }

    #[test]
    fn adjacent_hunks_get_no_separator() {
        let out = unified_diff_lines("t.txt", "one\na\ntwo\n", "ONE\na\nTWO\n");
        assert!(
            !out.iter().any(|(kind, _)| *kind == DiffLineKind::Separator),
            "{:?}",
            texts(&out),
        );
        assert!(texts(&out).contains(&"  a"));
    }

    #[test]
    fn identical_sides_render_only_headers() {
        let out = unified_diff_lines("t.txt", "same\n", "same\n");
        assert!(out.iter().all(|(kind, _)| *kind == DiffLineKind::Header));
    }
}
