//! Tests for the Editor backslash+Enter newline workaround.
//!
//! Terminals can't always distinguish `Enter` from `Shift+Enter` (absent
//! Kitty keyboard protocol), so the convention is to let users type `\`
//! immediately before `Enter` to insert a literal newline without
//! submitting. This file covers the full semantics of that workaround.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::Key;

fn editor() -> Editor {
    let mut e = Editor::new();
    e.set_focused(true);
    e
}

#[test]
fn backslash_is_inserted_immediately_without_buffering() {
    let mut e = editor();
    e.handle_input(&Key::char('\\'));
    // The backslash is visible right away — the workaround does not buffer.
    assert_eq!(e.get_text(), "\\");
}

#[test]
fn standalone_backslash_followed_by_enter_inserts_a_newline() {
    let mut e = editor();
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::enter());
    // The backslash is consumed; a newline takes its place.
    assert_eq!(e.get_text(), "\n");
}

#[test]
fn backslash_followed_by_other_characters_is_inserted_normally() {
    let mut e = editor();
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::char('x'));
    assert_eq!(e.get_text(), "\\x");
}

#[test]
fn enter_with_backslash_not_immediately_before_cursor_submits_normally() {
    let submitted = Rc::new(RefCell::new(false));
    let flag = Rc::clone(&submitted);

    let mut e = editor();
    e.on_submit = Some(Box::new(move |_text: &str| {
        *flag.borrow_mut() = true;
    }));

    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::char('x'));
    e.handle_input(&Key::enter());

    // Cursor was preceded by `x`, not `\`, so the submit path fires.
    assert!(
        *submitted.borrow(),
        "on_submit should have fired since the cursor was not immediately after a backslash",
    );
}

#[test]
fn enter_removes_only_the_single_trailing_backslash() {
    let mut e = editor();
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::char('\\'));
    assert_eq!(e.get_text(), "\\\\\\");

    e.handle_input(&Key::enter());
    // Only the final backslash gets converted into the newline.
    assert_eq!(e.get_text(), "\\\\\n");
}
