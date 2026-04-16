//! Regression tests for F16: the editor's cursor cell terminates with
//! a full SGR reset (`\x1b[0m`), not reverse-video-off only
//! (`\x1b[27m`). Without the full reset, foreground/background/
//! attribute styling that is open in the line text *before* the cursor
//! cell continues to apply to cells *after* the cursor cell on the
//! same row — visible bleed in real terminals when a styled line is
//! being edited.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::Key;
use aj_tui::tui::RenderHandle;

use support::themes::default_editor_theme;

/// Build a focused editor with default styling and zero horizontal
/// padding so render math is straightforward in the assertions below.
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

/// Find the first `\x1b[7m` reverse-video open in `s`, then return the
/// next ANSI SGR escape (the closing code) verbatim — e.g. `\x1b[0m` or
/// `\x1b[27m`. Used to assert exactly which closer the cursor cell
/// emits, regardless of which character or line it sits on.
fn next_sgr_after_reverse_open(s: &str) -> Option<String> {
    let open = s.find("\x1b[7m")?;
    let after = &s[open + "\x1b[7m".len()..];
    let next_esc = after.find('\x1b')?;
    let from_esc = &after[next_esc..];
    // Take through the next `m` inclusive.
    let m = from_esc.find('m')?;
    Some(from_esc[..=m].to_string())
}

// ---------------------------------------------------------------------------
// Cursor-at-end-of-line: highlighted-space form
// ---------------------------------------------------------------------------

#[test]
fn cursor_cell_at_end_of_line_terminates_with_full_sgr_reset() {
    // After `set_text("hello")` the cursor sits past `o`; the editor
    // emits a highlighted-space cell (`\x1b[7m \x1b[0m`).
    let mut e = editor_with_text("hello");
    let lines = e.render(20);
    let joined = lines.join("\n");

    let closer = next_sgr_after_reverse_open(&joined)
        .expect("expected a cursor cell with an SGR closer in the rendered output");
    assert_eq!(
        closer, "\x1b[0m",
        "cursor cell should close with a full SGR reset, not reverse-video-only; \
         see PORTING.md F16. Rendered: {joined:?}",
    );
}

// ---------------------------------------------------------------------------
// Cursor-on-grapheme: same closer, regardless of cell content
// ---------------------------------------------------------------------------

#[test]
fn cursor_cell_on_a_grapheme_terminates_with_full_sgr_reset() {
    // Position the cursor on a grapheme inside the line by typing
    // text and then walking left so a cell sits after the cursor on
    // the same row.
    let mut e = editor_with_text("ab");
    e.handle_input(&Key::left());
    let lines = e.render(20);
    let joined = lines.join("\n");

    let closer = next_sgr_after_reverse_open(&joined)
        .expect("expected a cursor cell with an SGR closer in the rendered output");
    assert_eq!(
        closer, "\x1b[0m",
        "cursor cell should close with a full SGR reset, not reverse-video-only; \
         see PORTING.md F16. Rendered: {joined:?}",
    );
}

// ---------------------------------------------------------------------------
// Behavioral check: styling open before the cursor must not bleed
// past it.
// ---------------------------------------------------------------------------

#[test]
fn styling_open_before_cursor_does_not_bleed_past_the_cursor_cell() {
    // Embed a red-foreground SGR open in the editor text, type a
    // trailing unstyled grapheme, then walk left so the cursor sits
    // on the trailing grapheme. With reverse-video-only closing
    // (`\x1b[27m`), the red would still be active for everything that
    // follows the cursor cell on the same row. With full reset
    // (`\x1b[0m`), the closer terminates red along with reverse-video.
    //
    // We assert via byte-shape: the closer used by the cursor cell is
    // `\x1b[0m`. (We inspect the cursor cell directly rather than
    // walking the post-cursor cells because the editor may follow up
    // with its own padding and reset bytes that would mask the bug
    // in a naive "is anything red?" check.)
    let mut e = editor_with_text("\x1b[31mab");
    e.handle_input(&Key::left());
    let lines = e.render(20);
    let joined = lines.join("\n");

    let closer = next_sgr_after_reverse_open(&joined)
        .expect("expected a cursor cell with an SGR closer in the rendered output");
    assert_eq!(
        closer, "\x1b[0m",
        "cursor cell must terminate with `\\x1b[0m` so a red `\\x1b[31m` open \
         in the line text does not bleed past the cursor; see PORTING.md F16. \
         Rendered: {joined:?}",
    );
}
