//! ANSI-related helpers shared by integration tests.
//!
//! Tests that assert on rendered content routinely need to strip SGR codes
//! before comparing strings, and to find the visible-column offset of a
//! substring inside a styled line. Keep those helpers here so individual
//! test files don't re-implement them (subtly differently) each time.

use aj_tui::ansi::visible_width;

/// Strip ANSI escape sequences from a string, leaving only visible
/// characters.
///
/// Recognizes:
/// - CSI sequences: `\x1b[` ... `<final byte 0x40..=0x7e>` (covers SGR `m`,
///   cursor movement, erase, etc.).
/// - OSC sequences: `\x1b]` ... terminated by BEL (`\x07`) or ST
///   (`\x1b\\`).
/// - APC / PM / SOS sequences (`\x1b_`, `\x1b^`, `\x1bX`) terminated by
///   BEL or ST. Our crate uses APC for private cursor markers, so tests
///   that strip output need to swallow them.
///
/// Unknown escape sequences are preserved rather than guessed at — callers
/// that care can add coverage as needed.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }

        match chars.peek().copied() {
            // CSI: consume up to and including a final byte in 0x40..=0x7e.
            Some('[') => {
                chars.next();
                for c2 in chars.by_ref() {
                    let code = u32::from(c2);
                    if (0x40..=0x7e).contains(&code) {
                        break;
                    }
                }
            }
            // OSC / APC / PM / SOS: consume up to and including BEL or ST
            // (ESC `\`). All four share the same terminator convention.
            Some(']') | Some('_') | Some('^') | Some('X') => {
                chars.next();
                while let Some(&c2) = chars.peek() {
                    chars.next();
                    if c2 == '\x07' {
                        break;
                    }
                    if c2 == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Unknown or stray ESC: keep it literal so we don't silently
            // swallow content the test might be asserting on.
            _ => out.push(c),
        }
    }
    out
}

/// Apply [`strip_ansi`] to every line.
pub fn plain_lines(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| strip_ansi(l)).collect()
}

/// [`plain_lines`], with trailing whitespace removed from each line.
///
/// Useful when a component pads rendered lines with trailing spaces and the
/// test only cares about the structural content.
pub fn plain_lines_trim_end(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| strip_ansi(l).trim_end().to_string())
        .collect()
}

/// Return the visible-column offset at which `needle` begins inside
/// `line`, ignoring any ANSI codes that precede it.
///
/// Panics if `needle` is not a substring of `line`. Use this to assert on
/// column alignment of styled output.
pub fn visible_index_of(line: &str, needle: &str) -> usize {
    let byte_index = line
        .find(needle)
        .unwrap_or_else(|| panic!("expected {:?} in {:?}", needle, line));
    visible_width(&line[..byte_index])
}
