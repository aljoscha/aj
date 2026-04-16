//! Tests for `Editor`'s PageUp / PageDown handling.
//!
//! Page scrolling is net-new behavior gained from routing the editor's
//! input dispatch through the `KeybindingsManager` registry: the
//! registry has carried `tui.editor.pageUp` / `tui.editor.pageDown`
//! defaults all along, but the previous hand-rolled `KeyCode` match
//! never bound them.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::Key;

/// Build an editor with `n` lines of content, focused, with submit
/// disabled so Enter inserts newlines rather than submitting.
fn editor_with_lines(n: usize) -> Editor {
    let mut e = Editor::new();
    e.disable_submit = true;
    e.set_focused(true);
    let text: Vec<String> = (0..n).map(|i| format!("line {i}")).collect();
    e.set_text(&text.join("\n"));
    e
}

#[test]
fn page_up_moves_the_cursor_up_by_max_visible_lines() {
    let mut e = editor_with_lines(30);
    e.set_max_visible_lines(8);
    // Cursor is at the end of line 29 after `set_text`.
    let (start_line, _) = e.cursor();
    assert_eq!(start_line, 29);

    e.handle_input(&Key::page_up());
    let (line_after_one, _) = e.cursor();
    assert_eq!(
        line_after_one,
        start_line - 8,
        "page up should move 8 visual lines up",
    );

    e.handle_input(&Key::page_up());
    let (line_after_two, _) = e.cursor();
    assert_eq!(line_after_two, start_line - 16);
}

#[test]
fn page_down_moves_the_cursor_down_by_max_visible_lines() {
    let mut e = editor_with_lines(30);
    e.set_max_visible_lines(8);
    // Move to the top first.
    for _ in 0..30 {
        e.handle_input(&Key::up());
    }
    let (start_line, _) = e.cursor();
    assert_eq!(start_line, 0);

    e.handle_input(&Key::page_down());
    let (line_after_one, _) = e.cursor();
    assert_eq!(line_after_one, 8);

    e.handle_input(&Key::page_down());
    let (line_after_two, _) = e.cursor();
    assert_eq!(line_after_two, 16);
}

#[test]
fn page_up_at_top_is_a_noop() {
    let mut e = editor_with_lines(5);
    e.set_max_visible_lines(8);
    // Move to the top.
    for _ in 0..5 {
        e.handle_input(&Key::up());
    }
    assert_eq!(e.cursor().0, 0);

    e.handle_input(&Key::page_up());
    assert_eq!(e.cursor().0, 0);
}

#[test]
fn page_down_at_bottom_is_a_noop() {
    let mut e = editor_with_lines(5);
    e.set_max_visible_lines(8);
    // Cursor starts at end of last line.
    assert_eq!(e.cursor().0, 4);

    e.handle_input(&Key::page_down());
    assert_eq!(e.cursor().0, 4);
}
