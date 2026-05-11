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
use support::strip_ansi;
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
// Trailing-whitespace preservation across wrap boundaries
// ---------------------------------------------------------------------------

/// Regression: typing a trailing space at the end of a wrapped row used
/// to disappear visually until a non-space character was typed after
/// it. The render path was wrapping with a trim-end variant that
/// dropped the trailing space from each wrapped row, so the rendered
/// row ended on the last non-space character even though the cursor
/// had advanced past the space.
#[test]
fn trailing_space_at_end_of_wrapped_line_is_visible_immediately() {
    // Width 16 → layout_width 15 (padding_x = 0 reserves one column for
    // the cursor). Input length 17, ending in a space, so the line
    // wraps and the wrap split lands at the last interior space —
    // leaving a chunk that ends with the user-typed trailing space.
    let mut e = editor_with_text("alpha beta gamma ");
    let width = 16;
    let lines = e.render(width);
    let content = content_rows(&lines);

    // The cursor lives on the row carrying the trailing space; the
    // user-typed text must appear *before* the cursor cell. Inspect
    // the prefix of the row before the first reverse-video escape and
    // assert it ends with the trailing space we typed. Asserting on
    // the whole row's plain text would be defeated by the right-side
    // inner padding the editor adds to fill `content_width`.
    let row = content
        .iter()
        .find(|r| r.contains("\x1b[7m"))
        .expect("expected a content row with a cursor");
    let cursor_start = row
        .find("\x1b[7m")
        .expect("expected a cursor reverse-video escape");
    let prefix = strip_ansi(&row[..cursor_start]);
    assert_eq!(
        prefix, "gamma ",
        "expected the cursor-row prefix (before the cursor cell) to \
         end with the trailing space the user typed; got {prefix:?} \
         from row {row:?}",
    );
}

/// Regression companion: cursor at end-of-line stays at the correct
/// visual column after the trailing space. The cursor cell renders as
/// a highlighted space — when the underlying space is preserved on the
/// row, the cursor appears one column past it, not on top of it.
#[test]
fn cursor_after_trailing_space_lands_past_the_space() {
    let mut e = editor_with_text("alpha beta gamma ");
    let width = 16;
    let lines = e.render(width);

    // Locate the content row that contains "gamma " — that's the row
    // the cursor should be on. The cursor cell is `\x1b[7m \x1b[0m`,
    // so by counting ANSI-stripped chars up to the start of the
    // reverse-video cell we can verify column placement.
    let content = content_rows(&lines);
    let row = content
        .iter()
        .find(|r| strip_ansi(r).contains("gamma "))
        .expect("expected a content row containing 'gamma '");

    // Strip everything from the first reverse-video escape onward, then
    // measure the visible width of the prefix.
    let cursor_start = row
        .find("\x1b[7m")
        .expect("expected a cursor reverse-video escape in the row");
    let prefix_visible_width = visible_width(&strip_ansi(&row[..cursor_start]));

    // "alpha beta " is 11 chars and lives on the first wrap chunk.
    // "gamma " is 6 chars on the second chunk. The cursor sits at the
    // column right after the trailing space — column 6.
    assert_eq!(
        prefix_visible_width, 6,
        "expected cursor at visible column 6 on the trailing-space row, \
         got column {prefix_visible_width}; row = {row:?}",
    );
}

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
