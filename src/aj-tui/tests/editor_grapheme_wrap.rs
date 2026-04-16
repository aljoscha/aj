//! Tests for grapheme-aware text wrapping inside the `Editor`.
//!
//! These cover the render-path behavior when the editor holds text with
//! wide glyphs (CJK, emoji) or mixed-width content: lines never overflow
//! the render width, split points land between graphemes (not inside
//! them), and the cursor renders correctly even when it sits on or
//! after a wide glyph.
//!
//! ### Note: content-width assertions, not padded-width equality
//!
//! The editor's `render(width)` emits content-only rows (no right-pad
//! to `width`). These tests therefore use `visible_width(line) <=
//! width` to catch wide-glyph overflow regressions, and assert on
//! exact split points where they matter (e.g. the CJK cases).

mod support;

use aj_tui::ansi::visible_width;
use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::Key;
use aj_tui::tui::RenderHandle;

use support::plain_lines;
use support::themes::default_editor_theme;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn content_rows(lines: &[String]) -> Vec<String> {
    if lines.len() <= 2 {
        return Vec::new();
    }
    lines[1..lines.len() - 1].to_vec()
}

fn plain_content_rows_trimmed(lines: &[String]) -> Vec<String> {
    plain_lines(&content_rows(lines))
        .into_iter()
        .map(|l| l.trim().to_string())
        .collect()
}

fn assert_no_row_exceeds_width(lines: &[String], width: usize) {
    for (i, line) in content_rows(lines).iter().enumerate() {
        let w = visible_width(line);
        assert!(
            w <= width,
            "content row {i} has visible width {w}, exceeds {width}: {line:?}",
        );
    }
}

// ---------------------------------------------------------------------------
// Wide glyphs in rendered content
// ---------------------------------------------------------------------------

#[test]
fn wraps_correctly_when_text_contains_wide_emojis() {
    // "Hello ✅ World" fits in width 20 (14 visible columns), so no wrap.
    let mut e = editor_with_text("Hello ✅ World");
    let lines = e.render(20);
    assert_no_row_exceeds_width(&lines, 20);
}

#[test]
fn wraps_long_text_with_emojis_at_correct_positions() {
    // Six ✅ → 12 columns, wraps at width 10.
    let mut e = editor_with_text("✅✅✅✅✅✅");
    let lines = e.render(10);
    assert_no_row_exceeds_width(&lines, 10);

    // Don't pin the exact split here — just that every row fits and
    // that every emoji made it through intact (no half-split graphemes).
    let rows = plain_content_rows_trimmed(&lines);
    let joined: String = rows.join("");
    assert_eq!(joined, "✅✅✅✅✅✅", "lost or split a grapheme: {rows:?}");
}

#[test]
fn wraps_cjk_characters_at_column_boundaries_not_mid_grapheme() {
    // "日本語テスト" = 6 CJK chars × 2 cols = 12 columns; render width 11.
    let mut e = editor_with_text("日本語テスト");
    let lines = e.render(11);
    assert_no_row_exceeds_width(&lines, 11);

    let rows = plain_content_rows_trimmed(&lines);
    assert_eq!(rows.len(), 2, "expected two content rows, got {rows:?}");
    assert_eq!(
        rows[0], "日本語テス",
        "first row should hold 5 CJK chars (10 cols)"
    );
    assert_eq!(rows[1], "ト", "remainder goes to the second row");
}

#[test]
fn handles_mixed_ascii_and_wide_characters_on_one_line() {
    // "Test ✅ OK 日本" = 4 + 1 + 2 + 1 + 2 + 1 + 4 = 15 visible columns.
    // Renders within width 16 on a single content row.
    let mut e = editor_with_text("Test ✅ OK 日本");
    let lines = e.render(16);

    let rows = plain_content_rows_trimmed(&lines);
    assert_eq!(rows.len(), 1, "expected a single content row, got {rows:?}");
    assert_eq!(rows[0], "Test ✅ OK 日本");
}

// ---------------------------------------------------------------------------
// Cursor rendering on wide glyphs
// ---------------------------------------------------------------------------

#[test]
fn renders_cursor_marker_when_line_contains_wide_glyphs() {
    // Cursor is at end after `set_text`, immediately after "B".
    let mut e = editor_with_text("A✅B");
    let lines = e.render(20);

    // The content row should carry the reverse-video cursor marker
    // regardless of what wide glyphs precede it.
    let rows = content_rows(&lines);
    assert!(
        rows.iter().any(|r| r.contains("\x1b[7m")),
        "expected reverse-video cursor marker in content: {rows:?}",
    );
}

// ---------------------------------------------------------------------------
// Width boundary behavior
// ---------------------------------------------------------------------------

#[test]
fn does_not_exceed_terminal_width_when_emoji_sits_on_the_wrap_boundary() {
    // "0123456789✅" = 10 ASCII + 2-wide emoji = 12 columns. With width 11
    // the emoji must move to the next row — it cannot half-wrap onto the
    // 11th column.
    let mut e = editor_with_text("0123456789✅");
    let lines = e.render(11);
    assert_no_row_exceeds_width(&lines, 11);
}

#[test]
fn cursor_sits_at_end_before_wrap_and_overflow_pushes_to_next_row() {
    // Fill the layout width exactly — with padding_x = 0 the renderer
    // reserves one column for the cursor, so render(10) leaves 9 cols
    // of content. Typing 9 chars fills it on a single row; one more
    // char must wrap onto a second row.
    let width = 10;
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_theme(default_editor_theme());
    e.set_padding_x(0);
    e.set_focused(true);

    for _ in 0..9 {
        e.handle_input(&Key::char('a'));
    }
    let lines = e.render(width);
    let rows = content_rows(&lines);
    assert_eq!(
        rows.len(),
        1,
        "expected a single content row before overflow, got {rows:?}",
    );
    assert!(
        rows[0].contains("\x1b[7m"),
        "cursor should be inline on the same row, got {:?}",
        rows[0],
    );

    // Ninth char filled layout width; the tenth must wrap.
    e.handle_input(&Key::char('a'));
    let lines = e.render(width);
    let rows = content_rows(&lines);
    assert_eq!(
        rows.len(),
        2,
        "expected the overflow char to wrap, got {rows:?}",
    );
}
