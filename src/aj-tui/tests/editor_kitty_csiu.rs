//! Tests for the `Editor`'s handling of Kitty CSI-u encoded keys whose
//! modifiers do not map to one of the editor's bindings.
//!
//! When the Kitty keyboard protocol is active, the terminal can deliver a
//! printable character keycode (e.g. ASCII `c` = 99) with a non-standard
//! modifier byte — `\x1b[99;9u` is `Super+c`. `crossterm` parses that byte
//! stream into a [`KeyEvent`] carrying `KeyCode::Char('c')` plus
//! `KeyModifiers::SUPER`.
//!
//! The editor must ignore such events rather than inserting the bare
//! character, otherwise a `Super+c` keyboard shortcut would type a literal
//! `c` into the buffer.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use aj_tui::keys::InputEvent;

fn editor() -> Editor {
    let mut e = Editor::new();
    e.disable_submit = true;
    e.set_focused(true);
    e
}

#[test]
fn ignores_printable_csi_u_sequences_with_unsupported_modifiers() {
    let mut e = editor();

    // Super+c: the printable keycode arrives alongside a modifier the
    // editor has no binding for. Expect a no-op, not a literal `c`.
    let super_c = InputEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER));
    e.handle_input(&super_c);

    assert_eq!(e.get_text(), "");
}

#[test]
fn ignores_printable_chars_combined_with_hyper_or_meta() {
    // Same rule applies across the three "extended" modifiers exposed by
    // `crossterm` when the Kitty protocol is active.
    let mut e = editor();

    let hyper_x = InputEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::HYPER));
    let meta_y = InputEvent::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::META));

    e.handle_input(&hyper_x);
    e.handle_input(&meta_y);

    assert_eq!(e.get_text(), "");
}

#[test]
fn still_inserts_printable_chars_with_only_shift() {
    // Sanity check: the guard must not be so strict it excludes plain
    // Shift+letter, which is how the terminal delivers uppercase.
    let mut e = editor();

    let shift_a = InputEvent::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
    e.handle_input(&shift_a);

    assert_eq!(e.get_text(), "A");
}
