//! Tests for the `Editor` public state accessors.
//!
//! Covers the two accessors the editor exposes for external inspection:
//! `cursor()` returning `(line, col)` (in chars, not bytes) and
//! `lines()` returning a defensive copy that callers can freely mutate
//! without disturbing the editor.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::Key;
use aj_tui::tui::RenderHandle;

fn editor() -> Editor {
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e
}

#[test]
fn cursor_reports_line_and_char_column() {
    let mut e = editor();
    assert_eq!(e.cursor(), (0, 0));

    e.handle_input(&Key::char('a'));
    e.handle_input(&Key::char('b'));
    e.handle_input(&Key::char('c'));
    assert_eq!(e.cursor(), (0, 3));

    e.handle_input(&Key::left());
    assert_eq!(e.cursor(), (0, 2));
}

#[test]
fn cursor_column_counts_characters_not_bytes() {
    // Multi-byte UTF-8 characters should each contribute exactly one
    // column to the reported cursor position.
    let mut e = editor();
    e.handle_input(&Key::char('ä'));
    e.handle_input(&Key::char('ö'));
    e.handle_input(&Key::char('ü'));

    assert_eq!(e.cursor(), (0, 3));
}

#[test]
fn lines_returns_a_defensive_copy() {
    let mut e = editor();
    e.set_text("a\nb");

    let mut lines = e.lines();
    assert_eq!(lines, vec!["a", "b"]);

    // Mutating the snapshot must not touch the editor.
    lines[0] = "mutated".to_string();
    assert_eq!(e.lines(), vec!["a", "b"]);
    assert_eq!(e.get_text(), "a\nb");
}
