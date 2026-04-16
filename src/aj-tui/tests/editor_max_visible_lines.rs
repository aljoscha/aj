//! Tests for `Editor::max_visible_lines` auto-sizing.
//!
//! Without an explicit `set_max_visible_lines` call, the editor sizes
//! its visible-row cap from the current terminal height as
//! `max(5, floor(rows * 0.3))`, mirroring the original framework. The
//! value is read from the `RenderHandle` published by the owning
//! `Tui`, so tests verify both the formula itself and the wiring that
//! delivers terminal dimensions through the handle.

mod support;

use aj_tui::components::editor::Editor;
use aj_tui::tui::{RenderHandle, Tui};

#[test]
fn max_visible_lines_falls_back_to_five_with_a_detached_handle() {
    // A standalone editor (constructed with [`RenderHandle::detached`],
    // never wired to a `Tui`) reads `terminal_rows() == 0` from its
    // handle. The auto-sizer falls back to the floor value of 5
    // rather than panicking or returning 0. After H6, "no handle" is
    // no longer representable; "detached handle" takes its place.
    let e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    assert_eq!(e.max_visible_lines(), 5);
}

#[test]
fn max_visible_lines_auto_sizes_from_terminal_rows_after_start() {
    // 30-row terminal → floor(30 * 0.3) = 9, max(5, 9) = 9.
    let terminal = support::VirtualTerminal::new(80, 30);
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().unwrap();

    let editor = Editor::new(tui.handle(), support::themes::default_editor_theme());
    tui.add_child(Box::new(editor));

    let editor_ref = tui.get_as::<Editor>(0).expect("editor lives at index 0");
    assert_eq!(
        editor_ref.max_visible_lines(),
        9,
        "30-row terminal should auto-size to 9 visible lines",
    );
}

#[test]
fn max_visible_lines_floor_is_five_on_short_terminals() {
    // 10-row terminal → floor(10 * 0.3) = 3, max(5, 3) = 5.
    // The floor protects narrow terminals from a one- or two-row
    // editor that can't fit a full keystroke history.
    let terminal = support::VirtualTerminal::new(80, 10);
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().unwrap();

    let editor = Editor::new(tui.handle(), support::themes::default_editor_theme());
    tui.add_child(Box::new(editor));

    let editor_ref = tui.get_as::<Editor>(0).expect("editor lives at index 0");
    assert_eq!(
        editor_ref.max_visible_lines(),
        5,
        "short terminal should clamp to the floor of 5",
    );
}

#[test]
fn explicit_set_max_visible_lines_wins_over_auto_size() {
    // Auto-sizing only kicks in when the editor has no explicit cap.
    // A test that calls `set_max_visible_lines(8)` should still see 8
    // even if the terminal would auto-size to a different value.
    let terminal = support::VirtualTerminal::new(80, 30);
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().unwrap();

    let mut editor = Editor::new(tui.handle(), support::themes::default_editor_theme());
    editor.set_max_visible_lines(8);
    tui.add_child(Box::new(editor));

    let editor_ref = tui.get_as::<Editor>(0).expect("editor lives at index 0");
    assert_eq!(
        editor_ref.max_visible_lines(),
        8,
        "explicit override should win over auto-sizing",
    );
}

#[test]
fn clear_max_visible_lines_re_enables_auto_sizing() {
    // The escape hatch: a caller that called `set_max_visible_lines`
    // and now wants auto-sizing back can call `clear_max_visible_lines`.
    let terminal = support::VirtualTerminal::new(80, 30);
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().unwrap();

    let mut editor = Editor::new(tui.handle(), support::themes::default_editor_theme());
    editor.set_max_visible_lines(99);
    tui.add_child(Box::new(editor));

    {
        let e = tui
            .get_mut_as::<Editor>(0)
            .expect("editor lives at index 0");
        assert_eq!(e.max_visible_lines(), 99);
        e.clear_max_visible_lines();
    }

    let e = tui.get_as::<Editor>(0).expect("editor lives at index 0");
    assert_eq!(
        e.max_visible_lines(),
        9,
        "clearing the override should fall back to auto-sized 9",
    );
}
