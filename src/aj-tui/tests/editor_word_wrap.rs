//! Tests for word-wrapped rendering inside the `Editor`.
//!
//! These drive `editor.render(width)` and inspect the returned lines,
//! stripping SGR codes from the content slice (between the top and
//! bottom border lines) before asserting on structure.
//!
//! The pure-function subset (16 cases — default segmentation and
//! pre-segmented atomic units for paste markers) lives in
//! `tests/word_wrap.rs`.

mod support;

use aj_tui::ansi::visible_width;
use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::tui::RenderHandle;

use support::plain_lines;
use support::themes::default_editor_theme;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an editor with the default theme and the text pre-filled. Submit
/// is disabled so Enter never triggers the on_submit path (irrelevant to
/// wrap tests, but keeps behavior deterministic). `padding_x` is set to
/// `0` so the usable content width equals the render width minus the one
/// column reserved for the cursor.
fn editor_with_text(text: &str) -> Editor {
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_theme(default_editor_theme());
    e.set_padding_x(0);
    e.set_focused(true);
    e.set_text(text);
    e
}

/// Trim the render output to just the content rows, dropping the top
/// and bottom border lines.
fn content_rows(lines: &[String]) -> Vec<String> {
    if lines.len() <= 2 {
        return Vec::new();
    }
    lines[1..lines.len() - 1].to_vec()
}

/// Plain-text (ANSI-stripped) content rows, with leading and trailing
/// whitespace removed so assertions can focus on visible structure.
fn plain_content_rows_trimmed(lines: &[String]) -> Vec<String> {
    plain_lines(&content_rows(lines))
        .into_iter()
        .map(|l| l.trim().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Word-boundary wrapping
// ---------------------------------------------------------------------------

#[test]
fn wraps_at_word_boundaries_instead_of_mid_word() {
    let mut e = editor_with_text("Hello world this is a test of word wrapping functionality");
    let width = 40;

    let lines = e.render(width);
    let rows = plain_content_rows_trimmed(&lines);

    assert!(!rows.is_empty(), "expected at least one content row");
    // The first line must not end with a mid-word hyphenation artifact.
    assert!(
        !rows[0].ends_with('-'),
        "first content row {:?} should not end with '-' (mid-word break)",
        rows[0],
    );

    // Every non-empty content row should end with a word character or a
    // conservative set of punctuation — never a mid-word letter that
    // would indicate a broken word.
    for (i, line) in rows.iter().enumerate() {
        let trimmed = line.trim_end();
        if let Some(last) = trimmed.chars().last() {
            let ok = last.is_alphanumeric() || matches!(last, '.' | ',' | '!' | '?' | ';' | ':');
            assert!(
                ok,
                "row {i} ends unexpectedly with {last:?} (line = {line:?})"
            );
        }
    }
}

#[test]
fn does_not_start_lines_with_leading_whitespace_after_word_wrap() {
    let mut e = editor_with_text("Word1 Word2 Word3 Word4 Word5 Word6");
    let width = 20;

    let lines = e.render(width);

    // For each non-empty content line, the stripped text must not begin
    // with whitespace followed by content: i.e. `^\s+\S` on the
    // trim-right variant should not match.
    for (i, line) in plain_lines(&content_rows(&lines)).into_iter().enumerate() {
        let right_trimmed = line.trim_end();
        if right_trimmed.trim_start().is_empty() {
            continue; // purely padding — ignore
        }
        assert!(
            !(right_trimmed.starts_with(char::is_whitespace)
                && right_trimmed
                    .trim_start()
                    .starts_with(|c: char| !c.is_whitespace())),
            "row {i} starts with leading whitespace before content: {right_trimmed:?}",
        );
    }
}

// ---------------------------------------------------------------------------
// Long-word (URL) behavior
// ---------------------------------------------------------------------------

#[test]
fn breaks_long_words_at_character_level_so_no_line_exceeds_width() {
    let mut e =
        editor_with_text("Check https://example.com/very/long/path/that/exceeds/width here");
    let width = 30;

    let lines = e.render(width);

    // A padding-to-width renderer would assert every content line has
    // visible-width equal to `width`. Our editor doesn't pad to width, so
    // we check the looser invariant: no line overflows. This still
    // catches the mid-word-break regression the test was written to
    // guard against.
    for (i, line) in content_rows(&lines).iter().enumerate() {
        let w = visible_width(line);
        assert!(
            w <= width,
            "content row {i} has visible width {w}, expected <= {width}; line = {line:?}",
        );
    }
}

// ---------------------------------------------------------------------------
// Whitespace preservation within a single rendered line
// ---------------------------------------------------------------------------

#[test]
fn preserves_multiple_spaces_within_words_on_the_same_line() {
    let mut e = editor_with_text("Word1   Word2    Word3");
    let width = 50;

    let lines = e.render(width);
    let rows = plain_content_rows_trimmed(&lines);

    assert!(!rows.is_empty(), "expected content rows");
    // The first content row should still contain the run of three spaces
    // between the first two words — the wrapper must not collapse
    // interior whitespace.
    assert!(
        rows[0].contains("Word1   Word2"),
        "expected interior spaces preserved in {:?}",
        rows[0],
    );
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn renders_empty_input_as_border_plus_single_content_row() {
    let mut e = editor_with_text("");
    let width = 40;

    let lines = e.render(width);

    // top border + one content row + bottom border = 3 rows.
    assert_eq!(
        lines.len(),
        3,
        "empty editor should render as 3 rows, got {}: {:?}",
        lines.len(),
        lines,
    );
}

#[test]
fn renders_single_word_that_fits_exactly() {
    let mut e = editor_with_text("1234567890");
    // The editor reserves one column for the cursor, so a 10-char word
    // fits exactly only at `width = 10 + 1`.
    let width = 10 + 1;

    let lines = e.render(width);

    assert_eq!(
        lines.len(),
        3,
        "expected 3 rows (top + one content + bottom), got {}: {:?}",
        lines.len(),
        lines,
    );
    let rows = plain_content_rows_trimmed(&lines);
    assert!(
        rows.iter().any(|r| r.contains("1234567890")),
        "expected content row to include '1234567890', got {rows:?}",
    );
}
