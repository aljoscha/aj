//! Styling for canonical file-edit display diffs.

use aj_agent::tool::{DiffDetails, DiffLineKind};
use aj_tui::style;

/// Styles an already-canonical display diff.
pub fn render_diff_details(diff: &DiffDetails) -> Vec<String> {
    diff.lines()
        .iter()
        .map(|line| match line.kind() {
            DiffLineKind::Header | DiffLineKind::Context | DiffLineKind::Separator => {
                style::dim(line.text())
            }
            DiffLineKind::Add => style::green(line.text()),
            DiffLineKind::Remove => style::red(line.text()),
        })
        .collect()
}

/// Computes and styles a display diff from file snapshots.
///
/// Tool results already contain [`DiffDetails`]. This compatibility helper is
/// retained for callers that still hold snapshots.
pub fn render_unified_diff(path: &str, before: &str, after: &str) -> Vec<String> {
    render_diff_details(&DiffDetails::new(path, before, after))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        let mut out = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
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
        String::from_utf8(out).expect("surviving bytes remain valid UTF-8")
    }

    #[test]
    fn renders_a_creation_diff_without_a_minus_header() {
        let out = render_unified_diff("foo.txt", "", "hello\nworld\n");
        let plain: Vec<_> = out.iter().map(|line| strip_ansi(line)).collect();
        assert_eq!(plain[0], "+++ b/foo.txt");
        assert!(plain.iter().any(|line| line == "+ hello"));
        assert!(plain.iter().any(|line| line == "+ world"));
    }

    #[test]
    fn renders_a_modification_diff_with_both_headers() {
        let out = render_unified_diff("foo.txt", "alpha\nbeta\n", "alpha\ngamma\n");
        let plain: Vec<_> = out.iter().map(|line| strip_ansi(line)).collect();
        assert_eq!(plain[0], "--- a/foo.txt");
        assert_eq!(plain[1], "+++ b/foo.txt");
        assert!(plain.iter().any(|line| line == "  alpha"));
        assert!(plain.iter().any(|line| line == "- beta"));
        assert!(plain.iter().any(|line| line == "+ gamma"));
    }
}
