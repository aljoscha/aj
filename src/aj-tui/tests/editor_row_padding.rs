//! Tests for the `Editor`'s row padding shape.
//!
//! The editor frames each visible row as
//! `left_padding + text + inner-pad-to-content-width + right_padding`,
//! where the two sides are `padding_x` spaces each. Without the right
//! pad and the inner-pad-to-width, a row whose text is shorter than
//! the editor's content area leaves the right-side cells untouched —
//! and on a render whose terminal background differs from the
//! editor's, those cells display the wrong color until the next full
//! repaint.
//!
//! These tests assert the visible width of every rendered visible
//! row equals the requested render width, regardless of where the
//! cursor sits and whether `padding_x` is zero or non-zero.

mod support;

use aj_tui::ansi::visible_width;
use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::tui::RenderHandle;

use support::strip_ansi;
use support::themes::identity_editor_theme;

const WIDTH: usize = 40;

fn editor(padding_x: usize) -> Editor {
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_theme(identity_editor_theme());
    e.set_padding_x(padding_x);
    e.set_focused(true);
    e
}

/// Return only the visible content rows (drop the top/bottom border
/// and any trailing autocomplete-popup rows).
fn content_rows(rendered: &[String]) -> Vec<&str> {
    // Editor renders: [top_border, ...content_rows..., bottom_border, popup?]
    // The borders are the rows that stripped to a run of `─`.
    let mut out = Vec::new();
    for line in &rendered[1..rendered.len() - 1] {
        let bare = strip_ansi(line);
        if bare.chars().all(|c| c == '─') {
            // safety: editor never emits two borders adjacent, but
            // be defensive.
            continue;
        }
        out.push(line.as_str());
    }
    out
}

#[test]
fn visible_row_visible_width_matches_render_width_at_padding_zero() {
    let mut e = editor(0);
    e.set_text("hello");

    let lines = e.render(WIDTH);
    let rows = content_rows(&lines);
    assert!(!rows.is_empty(), "expected at least one content row");

    for row in &rows {
        assert_eq!(
            visible_width(row),
            WIDTH,
            "content row should pad out to render width; got {:?}",
            strip_ansi(row),
        );
    }
}

#[test]
fn visible_row_visible_width_matches_render_width_at_padding_nonzero() {
    let mut e = editor(4);
    e.set_text("hello");

    let lines = e.render(WIDTH);
    let rows = content_rows(&lines);
    assert!(!rows.is_empty(), "expected at least one content row");

    for row in &rows {
        assert_eq!(
            visible_width(row),
            WIDTH,
            "content row with padding_x=4 should still pad out to \
             render width; got {:?}",
            strip_ansi(row),
        );
    }
}

#[test]
fn unfocused_short_row_pads_right_side() {
    // An unfocused editor has no cursor cell, so the row is just text
    // padded out to width. This is the path most likely to render
    // ragged-right under a row-background theme.
    let mut e = editor(2);
    e.set_focused(false);
    e.set_text("hi");

    let lines = e.render(WIDTH);
    let rows = content_rows(&lines);
    assert!(!rows.is_empty());

    let bare = strip_ansi(rows[0]);
    assert_eq!(
        bare.len(),
        WIDTH,
        "unfocused short row should pad out with spaces to the full \
         width; got {:?}",
        bare,
    );
    // Ends with at least the configured `right_padding` spaces.
    assert!(
        bare.ends_with("  "),
        "row should end with at least padding_x trailing spaces; got {:?}",
        bare,
    );
    // Begins with `left_padding` spaces.
    assert!(
        bare.starts_with("  "),
        "row should start with padding_x leading spaces; got {:?}",
        bare,
    );
}

#[test]
fn cursor_at_end_of_full_width_line_does_not_overflow_render_width() {
    // When the cursor sits at end-of-line and the line already fills
    // `content_width`, the cursor cell occupies a column inside the
    // padding. The right pad must shrink by one so the row's visible
    // width still equals the render width.
    let mut e = editor(3);
    // content_width = 40 - 3*2 = 34. Fill it exactly.
    let text: String = (0..34).map(|_| 'x').collect();
    e.set_text(&text);
    // Move cursor to end of line via re-set on a fresh editor (set_text
    // already places it past the last char).

    let lines = e.render(WIDTH);
    let rows = content_rows(&lines);
    assert!(!rows.is_empty());

    let row = rows[0];
    let bare = strip_ansi(row);
    assert_eq!(
        visible_width(row),
        WIDTH,
        "cursor-in-padding row should still match render width; got {:?}",
        bare,
    );
}

#[test]
fn empty_editor_visible_row_pads_to_render_width() {
    // Empty editor still renders a single visible row (the cursor's
    // line). Verify it pads to width.
    let mut e = editor(2);
    e.set_text("");

    let lines = e.render(WIDTH);
    let rows = content_rows(&lines);
    assert!(!rows.is_empty(), "empty editor should still emit a row");
    assert_eq!(
        visible_width(rows[0]),
        WIDTH,
        "empty editor row should pad out to width; got {:?}",
        strip_ansi(rows[0]),
    );
}
